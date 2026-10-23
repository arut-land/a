//! Identity from the loader's mapped image, independent of install paths.

use serde::{Serialize, Serializer};

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Identity {
    #[cfg(any(target_os = "linux", test))]
    ElfBuildId(Hex),
    #[cfg(target_os = "macos")]
    MachUuid(Hex),
    #[cfg(windows)]
    Pe {
        timestamp: u32,
        size: u32,
        pdb: Option<Hex>,
        age: Option<u32>,
    },
}

#[derive(Clone, Copy)]
pub(super) struct Hex {
    bytes: [u8; 64],
    len: usize,
}
impl Hex {
    #[cfg(test)]
    pub(super) fn maximal() -> Self {
        Self {
            bytes: [255; 64],
            len: 64,
        }
    }
    fn new(bytes: &[u8]) -> Option<Self> {
        if bytes.is_empty() || bytes.len() > 64 {
            return None;
        }
        let mut value = Self {
            bytes: [0; 64],
            len: bytes.len(),
        };
        value.bytes[..bytes.len()].copy_from_slice(bytes);
        Some(value)
    }
}
impl Serialize for Hex {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut text = [0; 128];
        for (i, byte) in self.bytes[..self.len].iter().enumerate() {
            text[i * 2] = b"0123456789abcdef"[usize::from(byte >> 4)];
            text[i * 2 + 1] = b"0123456789abcdef"[usize::from(byte & 15)];
        }
        serializer.serialize_str(std::str::from_utf8(&text[..self.len * 2]).expect("hex is ASCII"))
    }
}

#[cfg(target_os = "linux")]
pub(super) fn of(base: usize) -> Option<Identity> {
    struct Search {
        base: usize,
        identity: Option<Identity>,
    }
    unsafe extern "C" fn visit(
        info: *mut libc::dl_phdr_info,
        _: usize,
        data: *mut libc::c_void,
    ) -> libc::c_int {
        // SAFETY: dl_iterate_phdr lends these objects for this callback.
        let (info, search) = unsafe { (&*info, &mut *data.cast::<Search>()) };
        // SAFETY: the loader supplies dlpi_phnum readable program headers.
        let headers =
            unsafe { std::slice::from_raw_parts(info.dlpi_phdr, usize::from(info.dlpi_phnum)) };
        let bias = info.dlpi_addr as usize;
        let matches = headers
            .iter()
            .any(|h| h.p_type == libc::PT_LOAD && bias + h.p_vaddr as usize == search.base);
        if !matches {
            return 0;
        }
        for h in headers.iter().filter(|h| h.p_type == libc::PT_NOTE) {
            let start = h.p_vaddr as usize;
            let len = h.p_memsz as usize;
            // A program note need not be loaded. Read only a note wholly inside
            // a readable PT_LOAD; the loader lock keeps the mapping alive.
            if !headers.iter().any(|load| {
                load.p_type == libc::PT_LOAD
                    && load.p_flags & libc::PF_R != 0
                    && start >= load.p_vaddr as usize
                    && start
                        .checked_add(len)
                        .is_some_and(|end| end <= (load.p_vaddr + load.p_memsz) as usize)
            }) {
                continue;
            }
            // SAFETY: the range was checked against a readable loaded segment.
            let notes = unsafe { std::slice::from_raw_parts((bias + start) as *const u8, len) };
            if let Some(id) = build_id(notes) {
                search.identity = Some(Identity::ElfBuildId(id));
                break;
            }
        }
        1
    }
    let mut search = Search {
        base,
        identity: None,
    };
    // SAFETY: callback and context remain valid for this synchronous loader walk.
    unsafe { libc::dl_iterate_phdr(Some(visit), (&raw mut search).cast()) };
    search.identity
}

#[cfg(target_os = "linux")]
fn build_id(mut notes: &[u8]) -> Option<Hex> {
    while notes.len() >= 12 {
        let word =
            |offset| u32::from_ne_bytes(notes[offset..offset + 4].try_into().unwrap()) as usize;
        let (name, size, kind) = (word(0), word(4), word(8));
        let aligned = |len: usize| len.checked_add(3).map(|n| n & !3);
        let desc = 12usize.checked_add(aligned(name)?)?;
        let end = desc.checked_add(size)?;
        let next = desc.checked_add(aligned(size)?)?;
        if kind == 3 && notes.get(12..12usize.checked_add(name)?)? == b"GNU\0" {
            return Hex::new(notes.get(desc..end)?);
        }
        notes = notes.get(next..)?;
    }
    None
}

// libc retains these ABI definitions; no Mach calls or task-port bindings are needed.
#[allow(deprecated)]
#[cfg(target_os = "macos")]
pub(super) fn of(base: usize) -> Option<Identity> {
    // SAFETY: base came from dladdr for a frame in a loaded image. All supported
    // Apple targets use the native 64-bit Mach header and load-command layout.
    let header = unsafe { &*(base as *const libc::mach_header_64) };
    if header.magic != 0xfeed_facf {
        return None;
    }
    // SAFETY: the loader validated sizeofcmds and keeps its header mapped.
    let commands = unsafe {
        std::slice::from_raw_parts(
            (base + size_of::<libc::mach_header_64>()) as *const u8,
            header.sizeofcmds as usize,
        )
    };
    let mut offset = 0usize;
    for _ in 0..header.ncmds {
        let command = commands.get(offset..offset.checked_add(8)?)?;
        let kind = u32::from_ne_bytes(command[..4].try_into().ok()?);
        let size = u32::from_ne_bytes(command[4..].try_into().ok()?) as usize;
        if size < 8 {
            return None;
        }
        let end = offset.checked_add(size)?;
        let command = commands.get(offset..end)?;
        if kind == 0x1b {
            return Hex::new(command.get(8..24)?).map(Identity::MachUuid);
        }
        offset = end;
    }
    None
}

#[cfg(windows)]
pub(super) fn frame(ip: *mut std::ffi::c_void) -> super::Frame {
    use windows_sys::Win32::{
        Foundation::FreeLibrary,
        System::{
            Diagnostics::Debug::{
                IMAGE_DEBUG_DIRECTORY, IMAGE_DEBUG_TYPE_CODEVIEW, IMAGE_NT_HEADERS64,
            },
            LibraryLoader::{GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GetModuleHandleExW},
            SystemServices::IMAGE_DOS_HEADER,
        },
    };
    let mut module = std::ptr::null_mut();
    // SAFETY: FROM_ADDRESS interprets the argument as an address, not a string;
    // the resulting module reference keeps its headers alive until FreeLibrary.
    if unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
            ip.cast(),
            &mut module,
        )
    } == 0
    {
        return super::Frame::default();
    }
    let base = module as usize;
    // SAFETY: the module loader validated these native PE headers.
    let dos = unsafe { &*module.cast::<IMAGE_DOS_HEADER>() };
    let nt = unsafe { &*((base + dos.e_lfanew as usize) as *const IMAGE_NT_HEADERS64) };
    let timestamp = nt.FileHeader.TimeDateStamp;
    let size = nt.OptionalHeader.SizeOfImage;
    let mut pdb = None;
    let mut age = None;
    let directory = nt.OptionalHeader.DataDirectory[6];
    let start = directory.VirtualAddress as usize;
    let end = start.checked_add(directory.Size as usize);
    if start != 0 && end.is_some_and(|end| end <= size as usize) {
        let count = directory.Size as usize / size_of::<IMAGE_DEBUG_DIRECTORY>();
        for index in 0..count {
            // SAFETY: the debug directory range is contained in the pinned image.
            let debug = unsafe {
                ((base + start) as *const IMAGE_DEBUG_DIRECTORY)
                    .add(index)
                    .read_unaligned()
            };
            let at = debug.AddressOfRawData as usize;
            if debug.Type == IMAGE_DEBUG_TYPE_CODEVIEW
                && debug.SizeOfData >= 24
                && at.checked_add(24).is_some_and(|end| end <= size as usize)
            {
                // SAFETY: only the fixed RSDS prefix is read; never the PDB pathname.
                let data = unsafe { std::slice::from_raw_parts((base + at) as *const u8, 24) };
                if &data[..4] == b"RSDS" {
                    pdb = Hex::new(&data[4..20]);
                    age = Some(u32::from_le_bytes(data[20..24].try_into().unwrap()));
                    break;
                }
            }
        }
    }
    let frame = super::Frame {
        image: (u64::from(timestamp) << 32) | u64::from(size),
        offset: (ip as usize).saturating_sub(base) as u64,
        identity: Some(Identity::Pe {
            timestamp,
            size,
            pdb,
            age,
        }),
    };
    // SAFETY: releases exactly the reference acquired above.
    unsafe { FreeLibrary(module) };
    frame
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_identity_contains_only_bounded_hex() {
        assert!(Hex::new(&[]).is_none());
        assert!(Hex::new(&[0; 65]).is_none());
        assert_eq!(
            serde_json::to_string(&Hex::new(&[0, 15, 255]).unwrap()).unwrap(),
            "\"000fff\""
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn malformed_notes_cannot_escape_the_mapped_range() {
        for notes in [&[][..], &[255; 12], &[0; 12], &[1; 31]] {
            assert!(build_id(notes).is_none());
        }
    }
}
