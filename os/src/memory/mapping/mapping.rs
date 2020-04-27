//! Rv39 页表的构建 [`Mapping`]
//!
//! 许多方法返回 [`Result`]，如果出现错误会返回 `Err(message)`。设计目标是，此时如果终止线程，则不会产生后续问题。
//! 但是如果错误是由操作系统代码逻辑产生的，则会直接 panic。
//!
//! NOTE：实现支持缺页（TODO），但页表常驻内存

use crate::memory::{
    address::*,
    frame::{FrameTracker, FRAME_ALLOCATOR},
    mapping::{Flags, MapType, PageTable, PageTableEntry, PageTableTracker, Range, Segment},
    MemoryResult,
};
use alloc::{vec, vec::Vec};

#[derive(Default)]
/// 某个线程的内存映射关系
pub struct Mapping {
    /// 保存所有使用到的页表
    page_tables: Vec<PageTableTracker>,
    /// 根页表的物理页号
    root_ppn: PhysicalPageNumber,
}

impl Mapping {
    /// 将当前的映射加载到 `satp` 寄存器
    pub fn activate(&self) {
        // satp 低 27 位为页号，高 4 位为模式，8 表示 Sv39
        let new_satp = self.root_ppn.0 | (8 << 60);
        unsafe {
            // 将 new_satp 的值写到 satp 寄存器
            llvm_asm!("csrw satp, $0" :: "r"(new_satp) :: "volatile");
            // 刷新 TLB
            llvm_asm!("sfence.vma" :::: "volatile");
        }
    }

    /// 创建一个有根节点的映射
    pub fn new() -> MemoryResult<Mapping> {
        let root_table = PageTableTracker::new(FRAME_ALLOCATOR.lock().alloc()?);
        let root_ppn = root_table.page_number();
        Ok(Mapping {
            page_tables: vec![root_table],
            root_ppn,
        })
    }

    /// 加入一段映射，可能会相应地分配物理页面
    ///
    /// - `frame_limit`
    ///     最多分配的物理页面数量。
    /// - `init_data`
    ///     复制一段内存区域来初始化新的内存区域，其长度必须等于 `segment` 的大小。
    ///
    /// 返回所有新分配了帧的映射关系，数量不超过 `frame_limit`。
    ///
    /// 未被分配物理页面的虚拟页号暂时不会写入页表当中，它们会在发生 PageFault 后再建立页表项。
    pub fn map(
        &mut self,
        segment: &Segment,
        mut frame_limit: usize,
        init_data: Option<Range<VirtualPageNumber>>,
    ) -> MemoryResult<Vec<(VirtualPageNumber, FrameTracker)>> {
        match segment.map_type {
            // 线性映射，不需要考虑分配页面，只需将所有页面依次映射
            MapType::Linear => {
                println!("linear map {:x?}", segment.page_range);
                if init_data.is_some() {
                    // 线性映射应当只用于内核，不存在初始化的情况
                    unimplemented!();
                }
                for vpn in segment.iter() {
                    self.map_one(vpn, PhysicalPageNumber::from(vpn), segment.flags)?;
                }
                Ok(vec![])
            }
            // 按帧映射
            MapType::Framed => {
                if init_data.is_some() {
                    unimplemented!("TODO");
                }
                // 记录所有成功分配的页面映射
                let mut allocated_pairs = vec![];
                // 遍历需要映射的页号
                for vpn in segment.iter() {
                    if frame_limit > 0 {
                        frame_limit -= 1;
                        // 如果还有配额，继续分配帧进行映射
                        let frame: FrameTracker = FRAME_ALLOCATOR.lock().alloc()?;
                        println!("map {:x?} -> {:x?}", vpn, frame.page_number());
                        self.map_one(vpn, frame.page_number(), segment.flags)?;
                        allocated_pairs.push((vpn, frame));
                    } else {
                        // 没有配额则停止映射
                        println!("unmapped {:x?}", vpn);
                    }
                }
                Ok(allocated_pairs)
            }
        }
    }

    /// 找到给定虚拟页号的三级页表项
    ///
    /// 如果找不到对应的页表项，则会相应创建页表
    pub fn find_entry(&mut self, vpn: VirtualPageNumber) -> MemoryResult<&mut PageTableEntry> {
        // 从根页表开始向下查询
        // 这里不用 self.page_tables[0] 避免后面产生 borrow-check 冲突（我太菜了）
        let root_table: &mut PageTable = PhysicalAddress::from(self.root_ppn).deref_kernel();
        // print!("0: {:p} ", root_table);
        let mut pte = &mut root_table.entries[vpn.levels()[0]];
        // println!("[{}] = {:x?}", vpn.levels()[0], pte);
        for vpn_slice in &vpn.levels()[1..] {
            if pte.is_empty() {
                // 如果页表不存在，则需要分配一个新的页表
                let new_table = PageTableTracker::new(FRAME_ALLOCATOR.lock().alloc()?);
                let new_ppn = new_table.page_number();
                // 将新页表的页号写入当前的页表项
                *pte = PageTableEntry::new(new_ppn, Flags::VALID);
                // println!("write {:x?}", pte);
                // 保存页表
                self.page_tables.push(new_table);
            }
            // 进入下一级页表（使用偏移量来访问物理地址）
            pte = &mut pte.get_next_table().entries[*vpn_slice];
        }
        // 此时 pte 位于第三级页表
        Ok(pte)
    }

    /// 为给定的虚拟 / 物理页号建立映射关系
    fn map_one(
        &mut self,
        vpn: VirtualPageNumber,
        ppn: PhysicalPageNumber,
        flags: Flags,
    ) -> MemoryResult<()> {
        // 定位到页表项
        let entry = self.find_entry(vpn)?;
        assert!(entry.is_empty(), "virtual address is already mapped");
        // 页表项为空，则写入内容
        *entry = PageTableEntry::new(ppn, flags);
        Ok(())
    }

    /// 将给定虚拟页号的页表项标为 Invalid
    ///
    /// 用于 PageFault 中换出一个页面，会同时抹去其物理页号，防止错误访问。
    pub fn swap_out(&mut self, vpn: VirtualPageNumber) -> MemoryResult<()> {
        let entry = self.find_entry(vpn)?;
        assert!(
            entry.flags().contains(Flags::VALID),
            "page swapped out is not valid"
        );
        *entry = PageTableEntry::default();
        Ok(())
    }

    /// 设置给定虚拟页号的映射
    ///
    /// 用于 PageFault 中换入一个页面。
    pub fn swap_in(
        &mut self,
        vpn: VirtualPageNumber,
        ppn: PhysicalPageNumber,
        flags: Flags,
    ) -> MemoryResult<()> {
        let entry = self.find_entry(vpn)?;
        assert!(
            !entry.flags().contains(Flags::VALID),
            "page is already valid"
        );
        *entry = PageTableEntry::new(ppn, flags);
        Ok(())
    }
}
