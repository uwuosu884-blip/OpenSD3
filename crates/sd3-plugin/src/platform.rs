//! Native OS Folder Picker, File Picker, Clipboard Reader, and Global Soundbank
//! Registry for Windows 10/11 VST3/CLAP/Standalone and Linux.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GlobalBankRegistry {
    pub search_roots: Vec<PathBuf>,
    pub last_bank_path: Option<PathBuf>,
}

impl GlobalBankRegistry {
    pub fn load() -> Self {
        let Some(p) = registry_file_path() else {
            return Self::default();
        };
        fs::read_to_string(p)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let Some(p) = registry_file_path() else {
            return;
        };
        if let Some(parent) = p.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = fs::write(p, json);
        }
    }

    pub fn add_root(&mut self, root: &Path) {
        let norm = root.to_path_buf();
        let key = norm.to_string_lossy().to_ascii_lowercase();
        if !self
            .search_roots
            .iter()
            .any(|r| r.to_string_lossy().to_ascii_lowercase() == key)
        {
            self.search_roots.push(norm);
        }
    }
}

fn registry_file_path() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        if let Ok(appdata) = std::env::var("APPDATA") {
            return Some(PathBuf::from(appdata).join("OpenSD3").join("soundbanks.json"));
        }
        if let Ok(profile) = std::env::var("USERPROFILE") {
            return Some(
                PathBuf::from(profile)
                    .join("AppData")
                    .join("Roaming")
                    .join("OpenSD3")
                    .join("soundbanks.json"),
            );
        }
        None
    }
    #[cfg(not(target_os = "windows"))]
    {
        if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
            return Some(PathBuf::from(xdg).join("opensd3").join("soundbanks.json"));
        }
        if let Ok(home) = std::env::var("HOME") {
            return Some(
                PathBuf::from(home)
                    .join(".config")
                    .join("opensd3")
                    .join("soundbanks.json"),
            );
        }
        None
    }
}

// ===========================================================================
// Windows 10/11 Native Implementation (COM IFileOpenDialog + Win32 Clipboard)
// ===========================================================================

#[cfg(target_os = "windows")]
mod imp {
    use std::ffi::{c_void, OsStr, OsString};
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::path::PathBuf;
    use std::ptr::{null, null_mut};

    type HRESULT = i32;
    type ULONG = u32;
    type UINT = u32;
    type DWORD = u32;
    type BOOL = i32;
    type HWND = *mut c_void;
    type HANDLE = *mut c_void;
    type LPCWSTR = *const u16;
    type LPWSTR = *mut u16;

    const COINIT_APARTMENTTHREADED: DWORD = 0x2;
    const COINIT_DISABLE_OLE1DDE: DWORD = 0x4;
    const RPC_E_CHANGED_MODE: HRESULT = 0x8001_0106u32 as i32;
    const CLSCTX_INPROC_SERVER: DWORD = 0x1;

    const FOS_PICKFOLDERS: DWORD = 0x0000_0020;
    const FOS_FORCEFILESYSTEM: DWORD = 0x0000_0040;
    const FOS_PATHMUSTEXIST: DWORD = 0x0000_0800;
    const FOS_FILEMUSTEXIST: DWORD = 0x0000_1000;

    const SIGDN_FILESYSPATH: i32 = 0x8005_8000u32 as i32;
    const CF_UNICODETEXT: UINT = 13;

    const BIF_RETURNONLYFSDIRS: UINT = 0x0000_0001;
    const BIF_EDITBOX: UINT = 0x0000_0010;
    const BIF_NEWDIALOGSTYLE: UINT = 0x0000_0040;

    #[repr(C)]
    struct GUID {
        data1: u32,
        data2: u16,
        data3: u16,
        data4: [u8; 8],
    }

    const CLSID_FILE_OPEN_DIALOG: GUID = GUID {
        data1: 0xdc1c5a9c,
        data2: 0xe88a,
        data3: 0x4dde,
        data4: [0xa5, 0xa1, 0x60, 0xf8, 0x2a, 0x20, 0xae, 0xf7],
    };

    const IID_IFILE_OPEN_DIALOG: GUID = GUID {
        data1: 0xd57c7288,
        data2: 0xd4ad,
        data3: 0x4768,
        data4: [0xbe, 0x02, 0x9d, 0x96, 0x95, 0x32, 0xd9, 0x60],
    };

    #[repr(C)]
    struct COMDLG_FILTERSPEC {
        psz_name: LPCWSTR,
        psz_spec: LPCWSTR,
    }

    #[repr(C)]
    struct BROWSEINFOW {
        hwnd_owner: HWND,
        pidl_root: *const c_void,
        psz_display_name: LPWSTR,
        lpsz_title: LPCWSTR,
        ul_flags: UINT,
        lpfn: *const c_void,
        l_param: isize,
        i_image: i32,
    }

    #[repr(C)]
    struct IShellItem {
        lp_vtbl: *const IShellItemVtbl,
    }

    #[repr(C)]
    struct IShellItemVtbl {
        query_interface: unsafe extern "system" fn(*mut IShellItem, *const GUID, *mut *mut c_void) -> HRESULT,
        add_ref: unsafe extern "system" fn(*mut IShellItem) -> ULONG,
        release: unsafe extern "system" fn(*mut IShellItem) -> ULONG,
        bind_to_handler: *const c_void,
        get_parent: *const c_void,
        get_display_name: unsafe extern "system" fn(*mut IShellItem, i32, *mut LPWSTR) -> HRESULT,
        get_attributes: *const c_void,
        compare: *const c_void,
    }

    #[repr(C)]
    struct IFileOpenDialog {
        lp_vtbl: *const IFileOpenDialogVtbl,
    }

    #[repr(C)]
    struct IFileOpenDialogVtbl {
        // IUnknown (0..=2)
        query_interface: unsafe extern "system" fn(*mut IFileOpenDialog, *const GUID, *mut *mut c_void) -> HRESULT,
        add_ref: unsafe extern "system" fn(*mut IFileOpenDialog) -> ULONG,
        release: unsafe extern "system" fn(*mut IFileOpenDialog) -> ULONG,
        // IModalWindow (3)
        show: unsafe extern "system" fn(*mut IFileOpenDialog, HWND) -> HRESULT,
        // IFileDialog (4..=20+)
        set_file_types: unsafe extern "system" fn(*mut IFileOpenDialog, UINT, *const COMDLG_FILTERSPEC) -> HRESULT,
        set_file_type_index: unsafe extern "system" fn(*mut IFileOpenDialog, UINT) -> HRESULT,
        get_file_type_index: *const c_void,
        advise: *const c_void,
        unadvise: *const c_void,
        set_options: unsafe extern "system" fn(*mut IFileOpenDialog, DWORD) -> HRESULT,
        get_options: unsafe extern "system" fn(*mut IFileOpenDialog, *mut DWORD) -> HRESULT,
        set_default_folder: *const c_void,
        set_folder: *const c_void,
        get_folder: *const c_void,
        get_current_selection: *const c_void,
        set_file_name: *const c_void,
        get_file_name: *const c_void,
        set_title: unsafe extern "system" fn(*mut IFileOpenDialog, LPCWSTR) -> HRESULT,
        set_ok_button_label: unsafe extern "system" fn(*mut IFileOpenDialog, LPCWSTR) -> HRESULT,
        set_file_name_label: *const c_void,
        get_result: unsafe extern "system" fn(*mut IFileOpenDialog, *mut *mut IShellItem) -> HRESULT,
    }

    #[link(name = "ole32")]
    extern "system" {
        fn CoInitializeEx(pv_reserved: *mut c_void, dw_co_init: DWORD) -> HRESULT;
        fn CoUninitialize();
        fn CoCreateInstance(
            rclsid: *const GUID,
            p_unk_outer: *mut c_void,
            dw_cls_context: DWORD,
            riid: *const GUID,
            ppv: *mut *mut c_void,
        ) -> HRESULT;
        fn CoTaskMemFree(pv: *mut c_void);
    }

    #[link(name = "shell32")]
    extern "system" {
        fn SHBrowseForFolderW(lpbi: *mut BROWSEINFOW) -> *mut c_void;
        fn SHGetPathFromIDListW(pidl: *const c_void, psz_path: LPWSTR) -> BOOL;
    }

    #[link(name = "user32")]
    extern "system" {
        fn OpenClipboard(h_wnd_new_owner: HWND) -> BOOL;
        fn CloseClipboard() -> BOOL;
        fn GetClipboardData(u_format: UINT) -> HANDLE;
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GlobalLock(h_mem: HANDLE) -> *mut c_void;
        fn GlobalUnlock(h_mem: HANDLE) -> BOOL;
    }

    fn to_wide(s: &str) -> Vec<u16> {
        OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
    }

    unsafe fn pwstr_to_path(ptr: LPWSTR) -> Option<PathBuf> {
        if ptr.is_null() {
            return None;
        }
        let mut len = 0usize;
        while *ptr.add(len) != 0 {
            len += 1;
        }
        let slice = std::slice::from_raw_parts(ptr, len);
        let os = OsString::from_wide(slice);
        if os.is_empty() {
            None
        } else {
            Some(PathBuf::from(os))
        }
    }

    pub fn pick_folder_dialog() -> Option<PathBuf> {
        unsafe {
            let hr_init = CoInitializeEx(null_mut(), COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
            let should_uninit = hr_init >= 0;

            let mut dlg: *mut IFileOpenDialog = null_mut();
            let hr = CoCreateInstance(
                &CLSID_FILE_OPEN_DIALOG,
                null_mut(),
                CLSCTX_INPROC_SERVER,
                &IID_IFILE_OPEN_DIALOG,
                (&mut dlg as *mut *mut IFileOpenDialog).cast::<*mut c_void>(),
            );

            let result = if hr >= 0 && !dlg.is_null() {
                let vtbl = &*(*dlg).lp_vtbl;
                let mut opts: DWORD = 0;
                let _ = (vtbl.get_options)(dlg, &mut opts);
                let _ = (vtbl.set_options)(
                    dlg,
                    opts | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST,
                );
                let title = to_wide("Select Superior Drummer Soundbank (SL-*) or Toontrack Folder");
                let ok_lbl = to_wide("Select Soundbank");
                let _ = (vtbl.set_title)(dlg, title.as_ptr());
                let _ = (vtbl.set_ok_button_label)(dlg, ok_lbl.as_ptr());

                let mut chosen = None;
                if (vtbl.show)(dlg, null_mut()) >= 0 {
                    let mut item: *mut IShellItem = null_mut();
                    if (vtbl.get_result)(dlg, &mut item) >= 0 && !item.is_null() {
                        let ivtbl = &*(*item).lp_vtbl;
                        let mut pwstr: LPWSTR = null_mut();
                        if (ivtbl.get_display_name)(item, SIGDN_FILESYSPATH, &mut pwstr) >= 0 {
                            chosen = pwstr_to_path(pwstr);
                            CoTaskMemFree(pwstr.cast::<c_void>());
                        }
                        let _ = (ivtbl.release)(item);
                    }
                }
                let _ = (vtbl.release)(dlg);
                chosen
            } else if hr_init != RPC_E_CHANGED_MODE {
                browse_for_folder_fallback()
            } else {
                None
            };

            if should_uninit {
                CoUninitialize();
            }
            result
        }
    }

    unsafe fn browse_for_folder_fallback() -> Option<PathBuf> {
        let mut display_buf = [0u16; 260];
        let title = to_wide("Select Superior Drummer Soundbank (SL-*) or Toontrack Folder");
        let mut bi = BROWSEINFOW {
            hwnd_owner: null_mut(),
            pidl_root: null(),
            psz_display_name: display_buf.as_mut_ptr(),
            lpsz_title: title.as_ptr(),
            ul_flags: BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE | BIF_EDITBOX,
            lpfn: null(),
            l_param: 0,
            i_image: 0,
        };
        let pidl = SHBrowseForFolderW(&mut bi);
        if pidl.is_null() {
            return None;
        }
        let mut path_buf = [0u16; 1024];
        let ok = SHGetPathFromIDListW(pidl, path_buf.as_mut_ptr());
        CoTaskMemFree(pidl);
        if ok != 0 {
            pwstr_to_path(path_buf.as_mut_ptr())
        } else {
            None
        }
    }

    pub fn pick_kit_file_dialog() -> Option<PathBuf> {
        unsafe {
            let hr_init = CoInitializeEx(null_mut(), COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
            let should_uninit = hr_init >= 0;

            let mut dlg: *mut IFileOpenDialog = null_mut();
            let hr = CoCreateInstance(
                &CLSID_FILE_OPEN_DIALOG,
                null_mut(),
                CLSCTX_INPROC_SERVER,
                &IID_IFILE_OPEN_DIALOG,
                (&mut dlg as *mut *mut IFileOpenDialog).cast::<*mut c_void>(),
            );

            let mut chosen = None;
            if hr >= 0 && !dlg.is_null() {
                let vtbl = &*(*dlg).lp_vtbl;
                let mut opts: DWORD = 0;
                let _ = (vtbl.get_options)(dlg, &mut opts);
                let _ = (vtbl.set_options)(dlg, opts | FOS_FORCEFILESYSTEM | FOS_FILEMUSTEXIST);

                let f1_name = to_wide("Drum Kits & Soundbunches (*.obw;*.sfz;*.json;*.ttpw;*.wav)");
                let f1_spec = to_wide("*.obw;*.sfz;*.json;*.ttpw;*.wav");
                let f2_name = to_wide("All Files (*.*)");
                let f2_spec = to_wide("*.*");
                let filters = [
                    COMDLG_FILTERSPEC {
                        psz_name: f1_name.as_ptr(),
                        psz_spec: f1_spec.as_ptr(),
                    },
                    COMDLG_FILTERSPEC {
                        psz_name: f2_name.as_ptr(),
                        psz_spec: f2_spec.as_ptr(),
                    },
                ];
                let _ = (vtbl.set_file_types)(dlg, filters.len() as UINT, filters.as_ptr());
                let _ = (vtbl.set_file_type_index)(dlg, 1);

                let title = to_wide("Open .obw Soundbunch, .sfz Instrument, or kit.json");
                let _ = (vtbl.set_title)(dlg, title.as_ptr());

                if (vtbl.show)(dlg, null_mut()) >= 0 {
                    let mut item: *mut IShellItem = null_mut();
                    if (vtbl.get_result)(dlg, &mut item) >= 0 && !item.is_null() {
                        let ivtbl = &*(*item).lp_vtbl;
                        let mut pwstr: LPWSTR = null_mut();
                        if (ivtbl.get_display_name)(item, SIGDN_FILESYSPATH, &mut pwstr) >= 0 {
                            chosen = pwstr_to_path(pwstr);
                            CoTaskMemFree(pwstr.cast::<c_void>());
                        }
                        let _ = (ivtbl.release)(item);
                    }
                }
                let _ = (vtbl.release)(dlg);
            }

            if should_uninit {
                CoUninitialize();
            }
            chosen
        }
    }

    pub fn read_os_clipboard_text() -> Option<String> {
        unsafe {
            if OpenClipboard(null_mut()) == 0 {
                return None;
            }
            let hmem = GetClipboardData(CF_UNICODETEXT);
            let mut result = None;
            if !hmem.is_null() {
                let ptr = GlobalLock(hmem) as *const u16;
                if !ptr.is_null() {
                    let mut len = 0usize;
                    while len < 32_768 && *ptr.add(len) != 0 {
                        len += 1;
                    }
                    let slice = std::slice::from_raw_parts(ptr, len);
                    let s = String::from_utf16_lossy(slice).trim().to_owned();
                    if !s.is_empty() {
                        result = Some(s);
                    }
                    let _ = GlobalUnlock(hmem);
                }
            }
            let _ = CloseClipboard();
            result
        }
    }
}

// ===========================================================================
// Linux / Unix Implementation (`zenity` / `kdialog` + X11/Wayland Clipboard)
// ===========================================================================

#[cfg(not(target_os = "windows"))]
mod imp {
    use std::path::PathBuf;
    use std::process::Command;

    pub fn pick_folder_dialog() -> Option<PathBuf> {
        for (prog, args) in [
            (
                "zenity",
                &[
                    "--file-selection",
                    "--directory",
                    "--title=Select Superior Drummer Soundbank (SL-*) or Toontrack Folder",
                ][..],
            ),
            ("kdialog", &["--getexistingdirectory", "."][..]),
        ] {
            if let Ok(out) = Command::new(prog).args(args).output() {
                if out.status.success() {
                    let s = String::from_utf8_lossy(&out.stdout).trim().to_owned();
                    if !s.is_empty() {
                        return Some(PathBuf::from(s));
                    }
                }
            }
        }
        None
    }

    pub fn pick_kit_file_dialog() -> Option<PathBuf> {
        for (prog, args) in [
            (
                "zenity",
                &[
                    "--file-selection",
                    "--title=Open .obw, .sfz, or kit.json",
                    "--file-filter=Drum Kits | *.obw *.sfz *.json *.ttpw *.wav",
                    "--file-filter=All Files | *",
                ][..],
            ),
            ("kdialog", &["--getopenfilename", ".", "*.obw *.sfz *.json *.ttpw *.wav"][..]),
        ] {
            if let Ok(out) = Command::new(prog).args(args).output() {
                if out.status.success() {
                    let s = String::from_utf8_lossy(&out.stdout).trim().to_owned();
                    if !s.is_empty() {
                        return Some(PathBuf::from(s));
                    }
                }
            }
        }
        None
    }

    pub fn read_os_clipboard_text() -> Option<String> {
        for (prog, args) in [
            ("wl-paste", &["-n"][..]),
            ("xclip", &["-selection", "clipboard", "-o"][..]),
            ("xsel", &["--clipboard", "--output"][..]),
        ] {
            if let Ok(out) = Command::new(prog).args(args).output() {
                if out.status.success() {
                    let s = String::from_utf8_lossy(&out.stdout).trim().to_owned();
                    if !s.is_empty() {
                        return Some(s);
                    }
                }
            }
        }
        None
    }
}

pub use imp::{pick_folder_dialog, pick_kit_file_dialog, read_os_clipboard_text};
