//! Architecture-specific page-table entry formats.

use ax_memory_addr::{PAGE_SIZE_4K, PhysAddr, VirtAddr};
use page_table_generic::{MemAttributes, PageTableEntry, PteConfig, TableMeta};

/// Page-table metadata for the active target architecture.
#[derive(Clone, Copy)]
pub struct ArchPagingMeta;

impl TableMeta for ArchPagingMeta {
    type P = ArchPte;

    const PAGE_SIZE: usize = PAGE_SIZE_4K;

    cfg_if::cfg_if! {
        if #[cfg(any(target_arch = "riscv32", target_arch = "riscv64"))] {
            const LEVEL_BITS: &'static [usize] = &[9, 9, 9];
        } else {
            const LEVEL_BITS: &'static [usize] = &[9, 9, 9, 9];
        }
    }

    const MAX_BLOCK_LEVEL: usize = 3;
    const STRICT_ADDRESS_WIDTH: bool = false;

    fn flush(vaddr: Option<VirtAddr>) {
        crate::asm::flush_tlb(vaddr);
    }
}

cfg_if::cfg_if! {
    if #[cfg(target_arch = "x86_64")] {
        /// Page-table entry type for the active target architecture.
        pub type ArchPte = X64Pte;
    } else if #[cfg(any(target_arch = "riscv32", target_arch = "riscv64"))] {
        /// Page-table entry type for the active target architecture.
        pub type ArchPte = Rv64Pte;
    } else if #[cfg(target_arch = "aarch64")]{
        /// Page-table entry type for the active target architecture.
        pub type ArchPte = A64Pte;
    } else if #[cfg(target_arch = "loongarch64")] {
        /// Page-table entry type for the active target architecture.
        pub type ArchPte = La64Pte;
    }
}

#[cfg(target_arch = "x86_64")]
bitflags::bitflags! {
    #[derive(Clone, Copy, Debug)]
    struct X64PteFlags: u64 {
        const PRESENT = 1 << 0;
        const WRITABLE = 1 << 1;
        const USER = 1 << 2;
        const WRITE_THROUGH = 1 << 3;
        const NO_CACHE = 1 << 4;
        const DIRTY = 1 << 6;
        const HUGE_PAGE = 1 << 7;
        const GLOBAL = 1 << 8;
        const NO_EXECUTE = 1 << 63;
    }
}

/// x86_64 page-table entry.
#[cfg(target_arch = "x86_64")]
#[derive(Clone, Copy)]
#[repr(transparent)]
pub struct X64Pte(u64);

#[cfg(target_arch = "x86_64")]
impl X64Pte {
    const PHYS_ADDR_MASK: u64 = 0x000f_ffff_ffff_f000;

    fn flags(self) -> X64PteFlags {
        X64PteFlags::from_bits_truncate(self.0)
    }
}

#[cfg(target_arch = "x86_64")]
impl PageTableEntry for X64Pte {
    fn from_config(config: PteConfig) -> Self {
        if !config.valid {
            return Self(0);
        }
        if config.is_dir && !config.huge {
            return Self(
                (config.paddr.as_usize() as u64 & Self::PHYS_ADDR_MASK)
                    | (X64PteFlags::PRESENT | X64PteFlags::WRITABLE | X64PteFlags::USER).bits(),
            );
        }

        let mut flags = X64PteFlags::PRESENT;
        if config.writable {
            flags |= X64PteFlags::WRITABLE;
        }
        if config.lower {
            flags |= X64PteFlags::USER;
        }
        if matches!(
            config.mem_attr,
            MemAttributes::Device | MemAttributes::Uncached
        ) {
            flags |= X64PteFlags::NO_CACHE | X64PteFlags::WRITE_THROUGH;
        }
        if !config.executable {
            flags |= X64PteFlags::NO_EXECUTE;
        }
        if config.huge {
            flags |= X64PteFlags::HUGE_PAGE;
        }
        Self((config.paddr.as_usize() as u64 & Self::PHYS_ADDR_MASK) | flags.bits())
    }

    fn to_config(&self, is_dir: bool) -> PteConfig {
        let flags = self.flags();
        let valid = flags.contains(X64PteFlags::PRESENT);
        PteConfig {
            paddr: PhysAddr::from_usize((self.0 & Self::PHYS_ADDR_MASK) as usize),
            valid,
            read: valid,
            writable: flags.contains(X64PteFlags::WRITABLE),
            executable: valid && !flags.contains(X64PteFlags::NO_EXECUTE),
            lower: flags.contains(X64PteFlags::USER),
            dirty: flags.contains(X64PteFlags::DIRTY),
            global: flags.contains(X64PteFlags::GLOBAL),
            is_dir: is_dir && !flags.contains(X64PteFlags::HUGE_PAGE),
            huge: is_dir && flags.contains(X64PteFlags::HUGE_PAGE),
            mem_attr: if flags.contains(X64PteFlags::NO_CACHE) {
                MemAttributes::Uncached
            } else {
                MemAttributes::Normal
            },
        }
    }

    fn valid(&self) -> bool {
        self.flags().contains(X64PteFlags::PRESENT)
    }
}

#[cfg(target_arch = "x86_64")]
impl core::fmt::Debug for X64Pte {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("X64Pte")
            .field("raw", &self.0)
            .field("config", &self.to_config(false))
            .finish()
    }
}

#[cfg(any(target_arch = "riscv32", target_arch = "riscv64"))]
bitflags::bitflags! {
    #[derive(Clone, Copy, Debug)]
    struct RvPteFlags: u64 {
        const V = 1 << 0;
        const R = 1 << 1;
        const W = 1 << 2;
        const X = 1 << 3;
        const U = 1 << 4;
        const G = 1 << 5;
        const A = 1 << 6;
        const D = 1 << 7;
        #[cfg(feature = "xuantie-c9xx")]
        const SEC = 1 << 59;
        #[cfg(feature = "xuantie-c9xx")]
        const SH = 1 << 60;
        #[cfg(feature = "xuantie-c9xx")]
        const B = 1 << 61;
        #[cfg(feature = "xuantie-c9xx")]
        const C = 1 << 62;
        #[cfg(feature = "xuantie-c9xx")]
        const SO = 1 << 63;
    }
}

/// RISC-V Sv39/Sv48 page-table entry.
#[cfg(any(target_arch = "riscv32", target_arch = "riscv64"))]
#[derive(Clone, Copy)]
#[repr(transparent)]
pub struct Rv64Pte(u64);

#[cfg(any(target_arch = "riscv32", target_arch = "riscv64"))]
impl Rv64Pte {
    const PHYS_ADDR_MASK: u64 = (1 << 54) - (1 << 10);

    fn flags(self) -> RvPteFlags {
        RvPteFlags::from_bits_truncate(self.0)
    }

    fn paddr(self) -> PhysAddr {
        PhysAddr::from_usize(((self.0 & Self::PHYS_ADDR_MASK) << 2) as usize)
    }

    fn leaf_flags(config: PteConfig) -> RvPteFlags {
        let mut flags = RvPteFlags::V | RvPteFlags::A | RvPteFlags::D;
        if config.read || config.writable {
            flags |= RvPteFlags::R;
        }
        if config.writable {
            flags |= RvPteFlags::W;
        }
        if config.executable {
            flags |= RvPteFlags::X;
        }
        if config.lower {
            flags |= RvPteFlags::U;
        }
        #[cfg(feature = "xuantie-c9xx")]
        {
            if matches!(config.mem_attr, MemAttributes::Device) {
                flags |= RvPteFlags::SH | RvPteFlags::SO;
            } else if matches!(config.mem_attr, MemAttributes::Uncached) {
                flags |= RvPteFlags::SH | RvPteFlags::B;
            } else {
                flags |= RvPteFlags::SH | RvPteFlags::B | RvPteFlags::C;
            }
        }
        flags
    }
}

#[cfg(any(target_arch = "riscv32", target_arch = "riscv64"))]
impl PageTableEntry for Rv64Pte {
    fn from_config(config: PteConfig) -> Self {
        if !config.valid {
            return Self(0);
        }
        let paddr = (config.paddr.as_usize() as u64 >> 2) & Self::PHYS_ADDR_MASK;
        let flags = if config.is_dir && !config.huge {
            RvPteFlags::V
        } else {
            Self::leaf_flags(config)
        };
        Self(paddr | flags.bits())
    }

    fn to_config(&self, is_dir: bool) -> PteConfig {
        let flags = self.flags();
        let valid = flags.contains(RvPteFlags::V);
        let huge = is_dir && flags.intersects(RvPteFlags::R | RvPteFlags::X);
        PteConfig {
            paddr: self.paddr(),
            valid,
            read: flags.contains(RvPteFlags::R),
            writable: flags.contains(RvPteFlags::W),
            executable: flags.contains(RvPteFlags::X),
            lower: flags.contains(RvPteFlags::U),
            dirty: flags.contains(RvPteFlags::D),
            global: flags.contains(RvPteFlags::G),
            is_dir: is_dir && valid && !huge,
            huge,
            mem_attr: MemAttributes::Normal,
        }
    }

    fn valid(&self) -> bool {
        self.flags().contains(RvPteFlags::V)
    }
}

#[cfg(any(target_arch = "riscv32", target_arch = "riscv64"))]
impl core::fmt::Debug for Rv64Pte {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Rv64Pte")
            .field("raw", &self.0)
            .field("config", &self.to_config(false))
            .finish()
    }
}

#[cfg(target_arch = "aarch64")]
bitflags::bitflags! {
    #[derive(Clone, Copy, Debug)]
    struct A64DescriptorAttr: u64 {
        const VALID = 1 << 0;
        const NON_BLOCK = 1 << 1;
        const AP_EL0 = 1 << 6;
        const AP_RO = 1 << 7;
        const INNER = 1 << 8;
        const SHAREABLE = 1 << 9;
        const AF = 1 << 10;
        const NG = 1 << 11;
        const PXN = 1 << 53;
        const UXN = 1 << 54;
    }
}

#[cfg(target_arch = "aarch64")]
#[repr(u64)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum A64MemAttr {
    Device             = 0,
    Normal             = 1,
    NormalNonCacheable = 2,
}

#[cfg(target_arch = "aarch64")]
impl A64DescriptorAttr {
    const ATTR_INDEX_MASK: u64 = 0x1c;

    const fn from_mem_attr(idx: A64MemAttr) -> Self {
        let mut bits = (idx as u64) << 2;
        if matches!(idx, A64MemAttr::Normal | A64MemAttr::NormalNonCacheable) {
            bits |= Self::INNER.bits() | Self::SHAREABLE.bits();
        }
        Self::from_bits_retain(bits)
    }

    const fn mem_attr(self) -> Option<A64MemAttr> {
        let idx = (self.bits() & Self::ATTR_INDEX_MASK) >> 2;
        Some(match idx {
            0 => A64MemAttr::Device,
            1 => A64MemAttr::Normal,
            2 => A64MemAttr::NormalNonCacheable,
            _ => return None,
        })
    }
}

/// AArch64 VMSAv8-64 translation-table descriptor.
#[cfg(target_arch = "aarch64")]
#[derive(Clone, Copy)]
#[repr(transparent)]
pub struct A64Pte(u64);

#[cfg(target_arch = "aarch64")]
impl A64Pte {
    const PHYS_ADDR_MASK: u64 = 0x0000_ffff_ffff_f000;

    fn attr(self) -> A64DescriptorAttr {
        A64DescriptorAttr::from_bits_truncate(self.0)
    }

    fn leaf_attr(config: PteConfig) -> A64DescriptorAttr {
        let mem_attr = match config.mem_attr {
            MemAttributes::Device => A64MemAttr::Device,
            MemAttributes::Uncached => A64MemAttr::NormalNonCacheable,
            _ => A64MemAttr::Normal,
        };
        let mut attr = A64DescriptorAttr::from_mem_attr(mem_attr) | A64DescriptorAttr::AF;
        if config.read {
            attr |= A64DescriptorAttr::VALID;
        }
        if !config.writable {
            attr |= A64DescriptorAttr::AP_RO;
        }
        #[cfg(not(feature = "arm-el2"))]
        {
            if config.lower {
                attr |= A64DescriptorAttr::AP_EL0 | A64DescriptorAttr::PXN;
                if !config.executable {
                    attr |= A64DescriptorAttr::UXN;
                }
            } else {
                attr |= A64DescriptorAttr::UXN;
                if !config.executable {
                    attr |= A64DescriptorAttr::PXN;
                }
            }
        }
        #[cfg(feature = "arm-el2")]
        {
            if !config.executable {
                attr |= A64DescriptorAttr::UXN;
            }
        }
        attr
    }
}

#[cfg(target_arch = "aarch64")]
impl PageTableEntry for A64Pte {
    fn from_config(config: PteConfig) -> Self {
        if !config.valid {
            return Self(0);
        }
        if config.is_dir && !config.huge {
            let attr = A64DescriptorAttr::NON_BLOCK | A64DescriptorAttr::VALID;
            return Self((config.paddr.as_usize() as u64 & Self::PHYS_ADDR_MASK) | attr.bits());
        }

        let mut attr = Self::leaf_attr(config);
        if !config.huge {
            attr |= A64DescriptorAttr::NON_BLOCK;
        }
        Self((config.paddr.as_usize() as u64 & Self::PHYS_ADDR_MASK) | attr.bits())
    }

    fn to_config(&self, is_dir: bool) -> PteConfig {
        let attr = self.attr();
        let valid = attr.contains(A64DescriptorAttr::VALID);
        let huge = is_dir && !attr.contains(A64DescriptorAttr::NON_BLOCK);
        let mut config = PteConfig {
            paddr: PhysAddr::from_usize((self.0 & Self::PHYS_ADDR_MASK) as usize),
            valid,
            read: valid,
            writable: !attr.contains(A64DescriptorAttr::AP_RO),
            dirty: true,
            global: !attr.contains(A64DescriptorAttr::NG),
            is_dir: is_dir && valid && !huge,
            huge,
            mem_attr: match attr.mem_attr() {
                Some(A64MemAttr::Device) => MemAttributes::Device,
                Some(A64MemAttr::NormalNonCacheable) => MemAttributes::Uncached,
                _ => MemAttributes::Normal,
            },
            ..Default::default()
        };
        #[cfg(not(feature = "arm-el2"))]
        {
            config.lower = attr.contains(A64DescriptorAttr::AP_EL0);
            config.executable = if config.lower {
                !attr.contains(A64DescriptorAttr::UXN)
            } else {
                !attr.contains(A64DescriptorAttr::PXN)
            };
        }
        #[cfg(feature = "arm-el2")]
        {
            config.executable = !attr.contains(A64DescriptorAttr::UXN);
        }
        config
    }

    fn valid(&self) -> bool {
        self.attr().contains(A64DescriptorAttr::VALID)
    }
}

#[cfg(target_arch = "aarch64")]
impl core::fmt::Debug for A64Pte {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("A64Pte")
            .field("raw", &self.0)
            .field("config", &self.to_config(false))
            .finish()
    }
}

#[cfg(target_arch = "loongarch64")]
bitflags::bitflags! {
    #[derive(Clone, Copy, Debug)]
    struct LaPteFlags: u64 {
        const D = 1 << 1;
        const PLVL = 1 << 2;
        const PLVH = 1 << 3;
        const MATL = 1 << 4;
        const MATH = 1 << 5;
        const GH = 1 << 6;
        const P = 1 << 7;
        const W = 1 << 8;
        const G = 1 << 12;
        const NR = 1 << 61;
        const NX = 1 << 62;
    }
}

/// LoongArch64 page-table entry.
#[cfg(target_arch = "loongarch64")]
#[derive(Clone, Copy)]
#[repr(transparent)]
pub struct La64Pte(u64);

#[cfg(target_arch = "loongarch64")]
impl La64Pte {
    const PHYS_ADDR_MASK: u64 = 0x0000_ffff_ffff_f000;

    fn flags(self) -> LaPteFlags {
        LaPteFlags::from_bits_truncate(self.0)
    }

    fn paddr(self) -> PhysAddr {
        PhysAddr::from_usize((self.0 & Self::PHYS_ADDR_MASK) as usize)
    }

    fn leaf_flags(config: PteConfig) -> LaPteFlags {
        let mut flags = LaPteFlags::P;
        if !config.read {
            flags |= LaPteFlags::NR;
        }
        if config.writable {
            flags |= LaPteFlags::W | LaPteFlags::D;
        }
        if !config.executable {
            flags |= LaPteFlags::NX;
        }
        if config.lower {
            flags |= LaPteFlags::PLVL | LaPteFlags::PLVH;
        }
        match config.mem_attr {
            MemAttributes::Device => {}
            MemAttributes::Uncached => flags |= LaPteFlags::MATH,
            _ => flags |= LaPteFlags::MATL,
        }
        if config.huge {
            flags |= LaPteFlags::GH;
        }
        flags
    }
}

#[cfg(target_arch = "loongarch64")]
impl PageTableEntry for La64Pte {
    fn from_config(config: PteConfig) -> Self {
        if !config.valid {
            return Self(0);
        }
        let paddr = config.paddr.as_usize() as u64 & Self::PHYS_ADDR_MASK;
        if config.is_dir && !config.huge {
            return Self(paddr);
        }
        Self(paddr | Self::leaf_flags(config).bits())
    }

    fn to_config(&self, is_dir: bool) -> PteConfig {
        let flags = self.flags();
        let table = is_dir && !flags.contains(LaPteFlags::P) && self.paddr().as_usize() != 0;
        let valid = flags.contains(LaPteFlags::P) || table;
        PteConfig {
            paddr: self.paddr(),
            valid,
            read: valid && !flags.contains(LaPteFlags::NR),
            writable: flags.contains(LaPteFlags::W),
            executable: valid && !flags.contains(LaPteFlags::NX),
            lower: flags.contains(LaPteFlags::PLVL | LaPteFlags::PLVH),
            dirty: flags.contains(LaPteFlags::D),
            global: flags.contains(LaPteFlags::G),
            is_dir: table,
            huge: is_dir && flags.contains(LaPteFlags::GH),
            mem_attr: if !flags.contains(LaPteFlags::MATL) {
                if flags.contains(LaPteFlags::MATH) {
                    MemAttributes::Uncached
                } else {
                    MemAttributes::Device
                }
            } else {
                MemAttributes::Normal
            },
        }
    }

    fn valid(&self) -> bool {
        self.flags().contains(LaPteFlags::P) || (self.0 & Self::PHYS_ADDR_MASK) != 0
    }
}

#[cfg(target_arch = "loongarch64")]
impl core::fmt::Debug for La64Pte {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("La64Pte")
            .field("raw", &self.0)
            .field("config", &self.to_config(false))
            .finish()
    }
}
