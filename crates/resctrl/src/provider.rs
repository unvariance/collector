use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

pub trait FsProvider: Clone + Send + Sync + 'static {
    fn exists(&self, p: &Path) -> bool;
    fn create_dir(&self, p: &Path) -> io::Result<()>;
    fn remove_dir(&self, p: &Path) -> io::Result<()>;
    fn write_str(&self, p: &Path, data: &str) -> io::Result<()>;
    fn read_to_string(&self, p: &Path) -> io::Result<String>;
    fn check_can_open_for_write(&self, p: &Path) -> io::Result<()>;
    fn mount_resctrl(&self, target: &Path) -> io::Result<()>;
}

#[derive(Clone, Copy, Debug)]
pub struct RealFs;

impl FsProvider for RealFs {
    fn exists(&self, p: &Path) -> bool {
        p.exists()
    }

    fn create_dir(&self, p: &Path) -> io::Result<()> {
        fs::create_dir(p)
    }

    fn remove_dir(&self, p: &Path) -> io::Result<()> {
        fs::remove_dir(p)
    }

    fn write_str(&self, p: &Path, data: &str) -> io::Result<()> {
        // For resctrl tasks, the file must exist; do not create.
        let mut f = OpenOptions::new().write(true).open(p)?;
        f.write_all(data.as_bytes())
    }

    fn read_to_string(&self, p: &Path) -> io::Result<String> {
        fs::read_to_string(p)
    }

    fn check_can_open_for_write(&self, p: &Path) -> io::Result<()> {
        let _ = OpenOptions::new().write(true).open(p)?;
        Ok(())
    }

    fn mount_resctrl(&self, target: &Path) -> io::Result<()> {
        // Ensure target exists
        if !target.exists() {
            // create only the leaf directory, parents should exist on real systems
            // (e.g., /sys/fs). If they do not, this will fail which is fine.
            fs::create_dir(target)?;
        }
        #[cfg(target_os = "linux")]
        unsafe {
            use std::ffi::CString;
            let src = CString::new("resctrl").unwrap();
            let fstype = CString::new("resctrl").unwrap();
            let data: *const libc::c_void = std::ptr::null();
            let tgt_c = CString::new(target.as_os_str().to_string_lossy().as_bytes()).unwrap();
            let rc = libc::mount(src.as_ptr(), tgt_c.as_ptr(), fstype.as_ptr(), 0, data);
            if rc != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err(io::Error::from_raw_os_error(libc::ENOSYS))
        }
    }
}
