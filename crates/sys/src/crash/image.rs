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

// Static musl has no dladdr lookup; program headers cover static and dynamic images.
#[cfg(target_os = "linux")]
pub(super) fn frame(ip: usize) -> super::Frame {
    struct Search {
        ip: usize,
        frame: super::Frame,
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
        let Ok(bias) = usize::try_from(info.dlpi_addr) else {
            return 0;
        };
        let Some(relative) = search
            .ip
            .checked_sub(bias)
            .and_then(|value| u64::try_from(value).ok())
        else {
            return 0;
        };
        if !headers.iter().any(|h| {
            h.p_type == libc::PT_LOAD
                && relative >= h.p_vaddr
                && h.p_vaddr
                    .checked_add(h.p_memsz)
                    .is_some_and(|end| relative < end)
        }) {
            return 0;
        }
        let Some(base) = headers
            .iter()
            .filter(|h| h.p_type == libc::PT_LOAD)
            .map(|h| h.p_vaddr)
            .min()
            .and_then(|start| usize::try_from(start).ok())
            .and_then(|start| bias.checked_add(start))
        else {
            return 0;
        };
        let mut build = None;
        for h in headers.iter().filter(|h| h.p_type == libc::PT_NOTE) {
            let (Ok(start), Ok(len)) = (usize::try_from(h.p_vaddr), usize::try_from(h.p_memsz))
            else {
                continue;
            };
            let (Some(address), Some(end)) =
                (bias.checked_add(start), h.p_vaddr.checked_add(h.p_memsz))
            else {
                continue;
            };
            // A program note need not be loaded. Read only a note wholly inside
            // a readable PT_LOAD; the loader lock keeps the mapping alive.
            if !headers.iter().any(|load| {
                load.p_type == libc::PT_LOAD
                    && load.p_flags & libc::PF_R != 0
                    && h.p_vaddr >= load.p_vaddr
                    && load
                        .p_vaddr
                        .checked_add(load.p_memsz)
                        .is_some_and(|limit| end <= limit)
            }) {
                continue;
            }
            // SAFETY: the range was checked against a readable loaded segment.
            let notes = unsafe { std::slice::from_raw_parts(address as *const u8, len) };
            if let Some(id) = build_id(notes, h.p_align) {
                build = Some(id);
                break;
            }
        }
        search.frame = super::Frame {
            image: build.map_or(0, |id| crate::fnv1a(&id.bytes[..id.len])),
            offset: search.ip.saturating_sub(base) as u64,
            identity: build.map(Identity::ElfBuildId),
        };
        1
    }
    let mut search = Search {
        ip,
        frame: super::Frame {
            offset: ip as u64,
            ..super::Frame::default()
        },
    };
    // SAFETY: callback and context remain valid for this synchronous loader walk.
    unsafe { libc::dl_iterate_phdr(Some(visit), (&raw mut search).cast()) };
    search.frame
}

#[cfg(target_os = "linux")]
fn build_id(notes: &[u8], align: u64) -> Option<Hex> {
    use object::{NativeEndian, elf, read::elf::NoteIterator};
    let mut notes =
        NoteIterator::<elf::FileHeader64<NativeEndian>>::new(NativeEndian, align, notes).ok()?;
    while let Some(note) = notes.next().ok()? {
        if note.name() == elf::ELF_NOTE_GNU && note.n_type(NativeEndian) == elf::NT_GNU_BUILD_ID {
            return Hex::new(note.desc());
        }
    }
    None
}

#[cfg(target_os = "macos")]
pub(super) fn frame(ip: usize) -> super::Frame {
    use std::ffi::{CStr, c_void};
    let mut info = std::mem::MaybeUninit::<libc::Dl_info>::zeroed();
    // SAFETY: dladdr reads the address and fills the output buffer.
    let found = unsafe { libc::dladdr(ip as *const c_void, info.as_mut_ptr()) } != 0;
    // SAFETY: zeroed first, then filled by dladdr.
    let info = unsafe { info.assume_init() };
    let image = if found && !info.dli_fname.is_null() {
        // SAFETY: the loader owns this NUL-terminated name for this call.
        crate::fnv1a(unsafe { CStr::from_ptr(info.dli_fname) }.to_bytes())
    } else {
        0
    };
    super::Frame {
        image,
        offset: ip.saturating_sub(info.dli_fbase as usize) as u64,
        identity: found.then(|| of(info.dli_fbase as usize)).flatten(),
    }
}

#[cfg(target_os = "macos")]
fn of(base: usize) -> Option<Identity> {
    use object::{LittleEndian, macho::MachHeader64, read::macho::MachHeader};
    // SAFETY: base came from dladdr for a frame in a loaded image. Both supported
    // Apple architectures use native little-endian 64-bit Mach headers.
    let header = unsafe { &*(base as *const MachHeader64<LittleEndian>) };
    if !header.is_supported() {
        return None;
    }
    let len = size_of::<MachHeader64<LittleEndian>>() + header.sizeofcmds(LittleEndian) as usize;
    // SAFETY: the loader validated sizeofcmds and keeps its header mapped.
    let bytes = unsafe { std::slice::from_raw_parts(base as *const u8, len) };
    Hex::new(&header.uuid(LittleEndian, bytes, 0).ok()??).map(Identity::MachUuid)
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
            &raw mut module,
        )
    } == 0
    {
        return super::Frame::default();
    }
    let base = module as usize;
    // SAFETY: the module loader validated these native PE headers.
    let dos = unsafe { &*module.cast::<IMAGE_DOS_HEADER>() };
    let Ok(offset) = usize::try_from(dos.e_lfanew) else {
        // SAFETY: releases exactly the reference acquired above.
        unsafe { FreeLibrary(module) };
        return super::Frame::default();
    };
    // SAFETY: the loader validated the NT header at this offset in the pinned image.
    let nt = unsafe { &*((base + offset) as *const IMAGE_NT_HEADERS64) };
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
    #[cfg_attr(miri, ignore = "Miri does not provide loaded ELF images")]
    fn a_program_counter_identifies_its_image_without_symbol_lookup() {
        let pc = super::frame as *const () as usize;
        let frame = super::frame(pc);
        assert!(matches!(frame.identity, Some(Identity::ElfBuildId(_))));
        assert_ne!(frame.image, 0);
        assert!(frame.offset > 0 && frame.offset < pc as u64);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn only_the_gnu_build_id_descriptor_becomes_identity() {
        let mut notes = Vec::new();
        for (name, kind, value) in [
            (b"x\0".as_slice(), 42u32, &[9][..]),
            (b"GNU\0".as_slice(), 3, &[1, 2, 255][..]),
        ] {
            notes.extend_from_slice(&u32::try_from(name.len()).unwrap().to_ne_bytes());
            notes.extend_from_slice(&u32::try_from(value.len()).unwrap().to_ne_bytes());
            notes.extend_from_slice(&kind.to_ne_bytes());
            notes.extend_from_slice(name);
            while !notes.len().is_multiple_of(4) {
                notes.push(0);
            }
            notes.extend_from_slice(value);
            while !notes.len().is_multiple_of(4) {
                notes.push(0);
            }
        }
        assert_eq!(
            serde_json::to_string(&build_id(&notes, 4).unwrap()).unwrap(),
            "\"0102ff\""
        );
        for len in 0..notes.len() - 1 {
            assert!(build_id(&notes[..len], 4).is_none());
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn malformed_notes_cannot_escape_the_mapped_range() {
        for notes in [&[][..], &[255; 12], &[0; 12], &[1; 31]] {
            assert!(build_id(notes, 4).is_none());
        }
    }
}
