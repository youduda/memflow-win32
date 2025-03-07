use std::prelude::v1::*;

use super::{Win32Kernel, Win32ModuleListInfo};

use std::fmt;

use memflow::mem::virt_translate::*;
use memflow::prelude::v1::{Result, *};

// those only required when compiling cglue code
#[cfg(feature = "plugins")]
use memflow::cglue;

use super::Win32VirtualTranslate;

/// Exit status of a win32 process
pub type Win32ExitStatus = i32;

/// Process has not exited yet
pub const EXIT_STATUS_STILL_ACTIVE: i32 = 259;

/// EPROCESS ImageFileName byte length
pub const IMAGE_FILE_NAME_LENGTH: usize = 15;

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(::serde::Serialize))]
pub struct Win32ProcessInfo {
    pub base_info: ProcessInfo,

    // general information from eprocess
    pub dtb: Address,
    pub section_base: Address,
    pub ethread: Address,
    pub wow64: Address,

    // teb
    pub teb: Option<Address>,
    pub teb_wow64: Option<Address>,

    // peb
    pub peb_native: Option<Address>,
    pub peb_wow64: Option<Address>,

    // modules
    pub module_info_native: Option<Win32ModuleListInfo>,
    pub module_info_wow64: Option<Win32ModuleListInfo>,
}

impl Win32ProcessInfo {
    pub fn wow64(&self) -> Address {
        self.wow64
    }

    pub fn peb(&self) -> Option<Address> {
        if let Some(peb) = self.peb_wow64 {
            Some(peb)
        } else {
            self.peb_native
        }
    }

    pub fn peb_native(&self) -> Option<Address> {
        self.peb_native
    }

    pub fn peb_wow64(&self) -> Option<Address> {
        self.peb_wow64
    }

    /// Return the module list information of process native architecture
    ///
    /// If the process is a wow64 process, module_info_wow64 is returned, otherwise, module_info_native is
    /// returned.
    pub fn module_info(&self) -> Option<Win32ModuleListInfo> {
        if !self.wow64.is_null() {
            self.module_info_wow64
        } else {
            self.module_info_native
        }
    }

    pub fn module_info_native(&self) -> Option<Win32ModuleListInfo> {
        self.module_info_native
    }

    pub fn module_info_wow64(&self) -> Option<Win32ModuleListInfo> {
        self.module_info_wow64
    }

    pub fn translator(&self) -> Win32VirtualTranslate {
        Win32VirtualTranslate::new(self.base_info.sys_arch, self.dtb)
    }
}

#[cfg(feature = "plugins")]
cglue_impl_group!(Win32Process<T>, ProcessInstance, { VirtualTranslate });
#[cfg(feature = "plugins")]
cglue_impl_group!(Win32Process<T>, IntoProcessInstance, { VirtualTranslate });

pub struct Win32Process<T> {
    pub virt_mem: T,
    pub proc_info: Win32ProcessInfo,
    offset_eproc_exit_status: usize,
}

// TODO: can be removed i think
impl<T: Clone> Clone for Win32Process<T> {
    fn clone(&self) -> Self {
        Self {
            virt_mem: self.virt_mem.clone(),
            proc_info: self.proc_info.clone(),
            offset_eproc_exit_status: self.offset_eproc_exit_status,
        }
    }
}

impl<V: MemoryView> AsMut<V> for Win32Process<V> {
    fn as_mut(&mut self) -> &mut V {
        &mut self.virt_mem
    }
}

impl<T: MemoryView> MemoryView for Win32Process<T> {
    fn read_raw_iter(&mut self, data: ReadRawMemOps) -> Result<()> {
        self.virt_mem.read_raw_iter(data)
    }

    fn write_raw_iter(&mut self, data: WriteRawMemOps) -> Result<()> {
        self.virt_mem.write_raw_iter(data)
    }

    fn metadata(&self) -> MemoryViewMetadata {
        self.virt_mem.metadata()
    }
}

impl<T: PhysicalMemory, V: VirtualTranslate2> VirtualTranslate
    for Win32Process<VirtualDma<T, V, Win32VirtualTranslate>>
{
    fn virt_to_phys_list(
        &mut self,
        addrs: &[VtopRange],
        out: VirtualTranslationCallback,
        out_fail: VirtualTranslationFailCallback,
    ) {
        self.virt_mem.virt_to_phys_list(addrs, out, out_fail)
    }
}

// TODO: implement VAD and rollback to the old bound!
//impl<T: MemoryView> Process for Win32Process<T> {

impl<T: PhysicalMemory, V: VirtualTranslate2> Process
    for Win32Process<VirtualDma<T, V, Win32VirtualTranslate>>
{
    /// Retrieves virtual address translator for the process (if applicable)
    //fn vat(&mut self) -> Option<&mut Self::VirtualTranslateType>;

    /// Retrieves the state of the process
    fn state(&mut self) -> ProcessState {
        if let Ok(exit_status) = self.virt_mem.read::<Win32ExitStatus>(
            self.proc_info.base_info.address + self.offset_eproc_exit_status,
        ) {
            if exit_status == EXIT_STATUS_STILL_ACTIVE {
                ProcessState::Alive
            } else {
                ProcessState::Dead(exit_status)
            }
        } else {
            ProcessState::Unknown
        }
    }

    /// Walks the process' module list and calls the provided callback for each module
    fn module_address_list_callback(
        &mut self,
        target_arch: Option<&ArchitectureIdent>,
        mut callback: ModuleAddressCallback,
    ) -> memflow::error::Result<()> {
        let infos = [
            (
                self.proc_info.module_info_native,
                self.proc_info.base_info.sys_arch,
            ),
            (
                self.proc_info.module_info_wow64,
                self.proc_info.base_info.proc_arch,
            ),
        ];

        // Here we end up filtering out module_info_wow64 if it doesn't exist
        let iter = infos
            .iter()
            .filter(|(_, a)| {
                if let Some(ta) = target_arch {
                    a == ta
                } else {
                    true
                }
            })
            .cloned()
            .filter_map(|(info, arch)| info.zip(Some(arch)));

        self.module_address_list_with_infos_callback(iter, &mut callback)
            .map_err(From::from)
    }

    /// Retrieves a module by its structure address and architecture
    ///
    /// # Arguments
    /// * `address` - address where module's information resides in
    /// * `architecture` - architecture of the module. Should be either `ProcessInfo::proc_arch`, or `ProcessInfo::sys_arch`.
    fn module_by_address(
        &mut self,
        address: Address,
        architecture: ArchitectureIdent,
    ) -> memflow::error::Result<ModuleInfo> {
        let info = if architecture == self.proc_info.base_info.sys_arch {
            self.proc_info.module_info_native.as_mut()
        } else if architecture == self.proc_info.base_info.proc_arch {
            self.proc_info.module_info_wow64.as_mut()
        } else {
            None
        }
        .ok_or(Error(ErrorOrigin::OsLayer, ErrorKind::InvalidArchitecture))?;

        info.module_info_from_entry(
            address,
            self.proc_info.base_info.address,
            &mut self.virt_mem,
            architecture,
        )
        .map_err(From::from)
    }

    /// Retrieves address of the primary module structure of the process
    ///
    /// This will be the module of the executable that is being run, and whose name is stored in
    /// _EPROCESS::IMAGE_FILE_NAME
    fn primary_module_address(&mut self) -> memflow::error::Result<Address> {
        let mut ret = Err(Error(ErrorOrigin::OsLayer, ErrorKind::ModuleNotFound));
        let sptr = self as *mut Self;
        let callback = &mut |ModuleAddressInfo { address, arch }| {
            let s = unsafe { sptr.as_mut() }.unwrap();
            let info = if arch == s.proc_info.base_info.sys_arch {
                s.proc_info.module_info_native.as_mut()
            } else {
                s.proc_info.module_info_wow64.as_mut()
            }
            .unwrap();

            if let Ok((_, true)) = info
                .module_base_from_entry(address, &mut s.virt_mem, arch)
                .map(|b| (b, b == s.proc_info.section_base))
            {
                ret = Ok(address);
                false
            } else {
                true
            }
        };
        let proc_arch = self.proc_info.base_info.proc_arch;
        self.module_address_list_callback(Some(&proc_arch), callback.into())?;
        ret
    }

    fn module_import_list_callback(
        &mut self,
        info: &ModuleInfo,
        callback: ImportCallback,
    ) -> Result<()> {
        memflow::os::util::module_import_list_callback(&mut self.virt_mem, info, callback)
    }

    fn module_export_list_callback(
        &mut self,
        info: &ModuleInfo,
        callback: ExportCallback,
    ) -> Result<()> {
        memflow::os::util::module_export_list_callback(&mut self.virt_mem, info, callback)
    }

    fn module_section_list_callback(
        &mut self,
        info: &ModuleInfo,
        callback: SectionCallback,
    ) -> Result<()> {
        memflow::os::util::module_section_list_callback(&mut self.virt_mem, info, callback)
    }

    /// Retrieves the process info
    fn info(&self) -> &ProcessInfo {
        &self.proc_info.base_info
    }

    fn mapped_mem_range(
        &mut self,
        gap_size: imem,
        start: Address,
        end: Address,
        out: MemoryRangeCallback,
    ) {
        self.virt_mem.virt_page_map_range(gap_size, start, end, out)
    }
}

// TODO: replace the following impls with a dedicated builder
// TODO: add non cloneable thing
impl<'a, T: PhysicalMemory, V: VirtualTranslate2>
    Win32Process<VirtualDma<T, V, Win32VirtualTranslate>>
{
    pub fn with_kernel(kernel: Win32Kernel<T, V>, proc_info: Win32ProcessInfo) -> Self {
        let (phys_mem, vat) = kernel.virt_mem.into_inner();
        let virt_mem = VirtualDma::with_vat(
            phys_mem,
            proc_info.base_info.proc_arch,
            proc_info.translator(),
            vat,
        );

        Self {
            virt_mem,
            proc_info,
            offset_eproc_exit_status: kernel.offsets.eproc_exit_status(),
        }
    }

    /// Consumes this process, returning the underlying memory and vat objects
    pub fn into_inner(self) -> (T, V) {
        self.virt_mem.into_inner()
    }
}

impl<'a, T: PhysicalMemory, V: VirtualTranslate2>
    Win32Process<VirtualDma<Fwd<&'a mut T>, Fwd<&'a mut V>, Win32VirtualTranslate>>
{
    /// Constructs a new process by borrowing a kernel object.
    ///
    /// Internally this will create a `VirtualDma` object that also
    /// borrows the PhysicalMemory and Vat objects from the kernel.
    ///
    /// The resulting process object is NOT cloneable due to the mutable borrowing.
    ///
    /// When u need a cloneable Process u have to use the `::with_kernel` function
    /// which will move the kernel object.
    pub fn with_kernel_ref(kernel: &'a mut Win32Kernel<T, V>, proc_info: Win32ProcessInfo) -> Self {
        let (phys_mem, vat) = kernel.virt_mem.mem_vat_pair();
        let virt_mem = VirtualDma::with_vat(
            phys_mem.forward_mut(),
            proc_info.base_info.proc_arch,
            proc_info.translator(),
            vat.forward_mut(),
        );

        Self {
            virt_mem,
            proc_info,
            offset_eproc_exit_status: kernel.offsets.eproc_exit_status(),
        }
    }
}

impl<T: MemoryView> Win32Process<T> {
    fn module_address_list_with_infos_callback(
        &mut self,
        module_infos: impl Iterator<Item = (Win32ModuleListInfo, ArchitectureIdent)>,
        out: &mut ModuleAddressCallback,
    ) -> Result<()> {
        for (info, arch) in module_infos {
            let callback = &mut |address| out.call(ModuleAddressInfo { address, arch });
            info.module_entry_list_callback(self, arch, callback.into())?;
        }
        Ok(())
    }
}

impl<T> fmt::Debug for Win32Process<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self.proc_info)
    }
}
