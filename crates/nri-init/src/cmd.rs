use crate::error::{NriError, Result};
use crate::opts::Nsenter;
use std::io::ErrorKind;
use std::process::{Command, Stdio};

#[derive(Clone, Debug)]
pub enum Runner {
    Local,
    Nsenter(Nsenter),
}

impl Runner {
    pub fn run_capture(&self, program: &str, args: &[&str]) -> Result<(i32, String, String)> {
        let (prog, argv) = match self {
            Runner::Local => (
                program.to_string(),
                args.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            ),
            Runner::Nsenter(ns) => {
                let mut argv = vec![
                    "--target", "1", "--mount", "--uts", "--ipc", "--net", "--pid", "--", program,
                ];
                argv.extend(args);
                (
                    ns.path.clone(),
                    argv.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                )
            }
        };

        let output = match Command::new(&prog)
            .args(&argv)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
        {
            Ok(o) => o,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                // Treat missing binary as non-zero exit so higher layers can
                // gracefully fall back (e.g., NotSupported restart)
                let code = 127;
                let out = String::new();
                let err = e.to_string();
                return Ok((code, out, err));
            }
            Err(e) => return Err(NriError::Io(e)),
        };
        let code = output.status.code().unwrap_or(-1);
        let out = String::from_utf8_lossy(&output.stdout).to_string();
        let err = String::from_utf8_lossy(&output.stderr).to_string();
        Ok((code, out, err))
    }

    pub fn run_ok(&self, program: &str, args: &[&str]) -> Result<String> {
        let (code, out, err) = self.run_capture(program, args)?;
        if code == 0 {
            Ok(out)
        } else {
            Err(NriError::CommandFailed(format!(
                "{} {:?} -> {}: {}",
                program, args, code, err
            )))
        }
    }
}

pub fn default_runner(nsenter: &Option<Nsenter>) -> Runner {
    match nsenter {
        Some(ns) => Runner::Nsenter(ns.clone()),
        None => Runner::Local,
    }
}
