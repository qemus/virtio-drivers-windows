use core::{
    mem::MaybeUninit,
    num::NonZero,
    ops::{
        DerefMut,
        Deref,
    },
    pin::Pin,
    fmt,
    sync::atomic::{
        AtomicUsize,
        AtomicU8,
        AtomicU32,
        AtomicU64,
        AtomicBool,
        Ordering,
    },
    ptr::NonNull,
};

use alloc::{
    sync::{
        Arc,
        Weak,
    },
    vec::Vec,
    collections::BTreeMap,
};

use pin_init::*;
use winresult::STATUS;

use spin::rwlock::RwLock;

use crate::adapter::*;
use crate::device::*;
use crate::uapi::*;
use crate::function;

use modular_bitfield::prelude::*;

use wdk::dxgkrnl::{
    HANDLE,
    DXGK_PAGE_TABLE_LEVEL_DESC,
    DXGK_SEGMENTFLAGS,
    DXGK_SEGMENTDESCRIPTOR3,
    DXGK_SEGMENTDESCRIPTOR4,
};

const VIRTIO_GPU_PROCESS_TAG: u64 = u64::from_ne_bytes(*b"VGPUPROC");

#[derive(Specifier, Clone, Copy)]
#[bits = 2]
pub enum MemorySegment {
    ImplicitSystemMemory = 0,
    /* 3d resources / guest blobs */
    Aperture3D = 1,
    /* mappable blob resources (unused) */
    BlobMappable = 2,
    /* not mappable blob resources*/
    BlobHost3D = 3,
}

impl MemorySegment {
    pub const COUNT: u32 = 3;
    pub const SEGMENTS: [MemorySegment; Self::COUNT as usize] = [
        MemorySegment::Aperture3D,
        MemorySegment::BlobMappable,
        MemorySegment::BlobHost3D,
    ];

    #[inline]
    pub const fn mask(&self) -> u32 {
        1 << (self.index() as u32)
    }

    #[inline]
    pub const fn index(&self) -> u32 {
        match self {
            MemorySegment::ImplicitSystemMemory => unreachable!(),
            _ => *self as u32 - 1,
        }
    }

    #[inline]
    pub const fn phys(&self) -> u64 {
        match self {
            MemorySegment::ImplicitSystemMemory => unreachable!(),
            MemorySegment::Aperture3D   => 0x00C0000000,
            MemorySegment::BlobHost3D   => 0x0700000000,
            MemorySegment::BlobMappable => 0x2000000000,
        }
    }

    #[inline]
    pub const fn size(&self, shmem_len: u64) -> u64 {
        match self {
            MemorySegment::ImplicitSystemMemory => unreachable!(),
            MemorySegment::Aperture3D   => 1024 * 1024 * 1024,
            MemorySegment::BlobHost3D   => 16 * 1024 * 1024 * 1024,
            MemorySegment::BlobMappable => shmem_len,
        }
    }

    #[inline]
    pub fn flags(&self) -> DXGK_SEGMENTFLAGS {
        let mut flags: DXGK_SEGMENTFLAGS = unsafe { core::mem::zeroed() };

        match self {
            MemorySegment::ImplicitSystemMemory => unreachable!(),
            MemorySegment::Aperture3D => {
                flags.set_Aperture(true);
                flags.set_CacheCoherent(true);
                flags.set_DirectFlip(true);
            },
            MemorySegment::BlobHost3D => {
                flags.set_CacheCoherent(true);
                flags.set_CpuVisible(true);
                flags.set_DirectFlip(true);
            },
            MemorySegment::BlobMappable => {
                flags.set_CacheCoherent(true);
                flags.set_DirectFlip(true);
                // FIXME: this isn't really a CpuVisible segment, but allocating from it fails otherwise.
                // Apparently, VirtualBox authors encountered the same problem:
                // https://github.com/VirtualBox/virtualbox/blob/499d5a317f23448903c7662a38baf957de37ddf4/src/VBox/Additions/win/Graphics/Video/mp/wddm/VBoxMPWddm.cpp#L2339
                flags.set_CpuVisible(true);
            },
        };

        flags
    }

    pub fn fill_desc3(&self, shmem_start: u64, shmem_len: u64, desc: &mut DXGK_SEGMENTDESCRIPTOR3) {
        desc.BaseAddress.QuadPart = self.phys() as _;
        desc.Size = self.size(shmem_len) as _;
        desc.CommitLimit = self.size(shmem_len) as _;
        desc.Flags = self.flags();

        if matches!(self, MemorySegment::BlobMappable) {
            desc.CpuTranslatedAddress.QuadPart = shmem_start as _;
        }
    }

    pub fn fill_desc4(&self, shmem_start: u64, shmem_len: u64, desc: &mut DXGK_SEGMENTDESCRIPTOR4) {
        desc.BaseAddress.QuadPart = self.phys() as _;
        desc.Size = self.size(shmem_len) as _;
        desc.CommitLimit = self.size(shmem_len) as _;
        desc.Flags = self.flags();

        if matches!(self, MemorySegment::BlobMappable) {
            desc.__bindgen_anon_1.CpuTranslatedAddress.QuadPart = shmem_start as _;
        }
    }
}

#[bitfield(bytes = 8)]
pub struct PageTableEntry {
    pub valid: bool,
    #[bits = 2]
    pub segment: MemorySegment,
    pub address: B61,
}

pub const PAGE_TABLE_DESC: [DXGK_PAGE_TABLE_LEVEL_DESC; 3] = [
    DXGK_PAGE_TABLE_LEVEL_DESC {
        PageTableIndexBitCount: 12,
        PageTableSegmentId: 0,
        PagingProcessPageTableSegmentId: 0,
        PageTableSizeInBytes: 4096 * size_of::<PageTableEntry>() as u32,
        PageTableAlignmentInBytes: 4096,
    },
    DXGK_PAGE_TABLE_LEVEL_DESC {
        PageTableIndexBitCount: 12,
        PageTableSegmentId: 0,
        PagingProcessPageTableSegmentId: 0,
        PageTableSizeInBytes: 4096 * size_of::<PageTableEntry>() as u32,
        PageTableAlignmentInBytes: 4096,
    },
    DXGK_PAGE_TABLE_LEVEL_DESC {
        PageTableIndexBitCount: 12,
        PageTableSegmentId: 0,
        PagingProcessPageTableSegmentId: 0,
        PageTableSizeInBytes: 4096 * size_of::<PageTableEntry>() as u32,
        PageTableAlignmentInBytes: 4096,
    },
];

const _: () = assert!(core::mem::size_of::<PageTableEntry>() == 8);

impl MemorySegment {
    fn from_pte_segment(segment: u8) -> Self {
        match segment {
            0 => Self::ImplicitSystemMemory,
            1 => Self::Aperture3D,
            2 => Self::BlobMappable,
            3 => Self::BlobHost3D,
            _ => unreachable!("DXGK_PTE segment is a 2-bit field"),
        }
    }
}

impl PageTableEntry {
    fn from_dxgk_pte(entry: &DXGK_PTE) -> Self {
        let mut result = Self::new();
        let valid = entry.Flags().Valid();

        result.set_valid(valid);
        if valid {
            result.set_segment(MemorySegment::from_pte_segment(entry.Flags().Segment()));
            result.set_address(entry.PageAddress());
        }

        result
    }
}

/// Apply a WDDM GPU page-table update to the CPU-visible software page table.
///
/// VirtIO-GPU submissions reference host resources rather than dereferencing
/// guest GPU virtual addresses, so the guest page tables are bookkeeping for
/// VidMm. We still have to maintain the page-table memory exactly as advertised
/// by DXGK_GPUMMUCAPS. In particular, paging-process initialization arrives
/// with pDmaBuffer == NULL and must be completed synchronously.
pub fn update_page_table_cpu(update: &DXGK_BUILDPAGINGBUFFER_UPDATEPAGETABLE) -> Result<(), NtStatus> {
    if update.UpdateMode != DXGK_PAGETABLEUPDATEMODE::DXGK_PAGETABLEUPDATE_CPU_VIRTUAL {
        error!("{}: unsupported page-table update mode: {:?}", function!(), update.UpdateMode);
        return Err(NtStatus(STATUS::NOT_SUPPORTED));
    }

    let level = update.PageTableLevel as usize;
    let Some(level_desc) = PAGE_TABLE_DESC.get(level) else {
        error!("{}: invalid page-table level {}", function!(), update.PageTableLevel);
        return Err(NtStatus(STATUS::INVALID_PARAMETER));
    };

    if update.NumPageTableEntries == 0 {
        return Ok(());
    }

    if update.pPageTableEntries.is_null() {
        error!("{}: page-table entry source is null", function!());
        return Err(NtStatus(STATUS::INVALID_PARAMETER));
    }

    let page_table = unsafe { update.PageTableAddress.__bindgen_anon_1.CpuVirtual } as *mut PageTableEntry;
    if page_table.is_null() {
        error!("{}: CPU virtual page-table address is null", function!());
        return Err(NtStatus(STATUS::INVALID_PARAMETER));
    }

    let table_entry_count = 1usize
        .checked_shl(level_desc.PageTableIndexBitCount)
        .ok_or(NtStatus(STATUS::INVALID_PARAMETER))?;
    let start = update.StartIndex as usize;
    let count = update.NumPageTableEntries as usize;
    let end = start
        .checked_add(count)
        .ok_or(NtStatus(STATUS::INVALID_PARAMETER))?;

    if end > table_entry_count {
        error!(
            "{}: page-table update [{}..{}) exceeds level {} entry count {}",
            function!(),
            start,
            end,
            update.PageTableLevel,
            table_entry_count,
        );
        return Err(NtStatus(STATUS::INVALID_PARAMETER));
    }

    for i in 0..count {
        let src = unsafe { &*update.pPageTableEntries.add(i) };
        let dst = unsafe { page_table.add(start + i) };
        unsafe { dst.write(PageTableEntry::from_dxgk_pte(src)); }
    }

    Ok(())
}

#[repr(C)]
#[derive(Tagged)]
#[tagged(VIRTIO_GPU_PROCESS_TAG)]
pub struct Process {
    pub tag: u64,
    dxg_process: HANDLE,
    // alloc_va_map: RwLock<HashMap<D3DGPU_VIRTUAL_ADDRESS, Weak<Allocation>>>
    // TODO: page table
    // TODO: process name
}

impl Process {
    pub fn new(dxg_process: HANDLE) -> Result<Arc<Self>, NtStatus> {
        Ok(Arc::try_new(Self {
            tag: VIRTIO_GPU_PROCESS_TAG,
            dxg_process,
        })?)
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.tag = 0;
    }
}
