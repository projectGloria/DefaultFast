//! On-demand elevation for the one thing that needs admin: the HKLM class default of a core
//! Windows type. The app itself always runs as the user; this relaunches the same exe via
//! ShellExecute "runas" in a headless helper mode and waits for it.
//!
//! The helper trusts nothing on its command line: extensions must be in the known-good
//! table, values must be that table's ProgID (fix) or a plain ProgID / "-" (restore), and the
//! registry layer's allowlist still limits writes to HKLM\SOFTWARE\Classes\.<ext> itself.
use crate::{
    backup::Op,
    error::{Error, Result},
    logfile,
    registry::{Hive, Registry, Value, WinRegistry},
    repair::{self, KnownGood},
    shell,
};

fn plain_prog_id(v: &str) -> bool {
    !v.is_empty() && v.len() < 128 && v.chars().all(|c| c.is_ascii_alphanumeric() || "._-+".contains(c))
}

/// Helper entry point: `--elevated-fix .bat .cmd` or `--elevated-set .bat=batfile .cmd=-`.
pub fn main(args: &[String]) -> i32 {
    if !shell::is_elevated() {
        return 2;
    }
    let reg = WinRegistry;
    let mut ok = true;
    let mode = args.first().map(String::as_str).unwrap_or_default();
    for a in args.iter().skip(1) {
        let r = match mode {
            "--elevated-fix" => match repair::known(a) {
                Some(k) => repair::fix_machine(&reg, k),
                None => Err(Error::Forbidden(a.clone())),
            },
            "--elevated-set" => {
                let (ext, value) = a.split_once('=').unwrap_or((a, ""));
                match repair::known(ext) {
                    Some(k) if value == "-" => reg.delete_value(Hive::LocalMachine, &format!("SOFTWARE\\Classes\\{}", k.ext), ""),
                    Some(k) if plain_prog_id(value) => reg.set(Hive::LocalMachine, &format!("SOFTWARE\\Classes\\{}", k.ext), "", &Value::Sz(value.to_owned())),
                    _ => Err(Error::Forbidden(a.clone())),
                }
            }
            _ => Err(Error::Forbidden(mode.to_owned())),
        };
        if let Err(e) = r {
            logfile::write(&format!("elevated helper: {a}: {e}"));
            ok = false;
        }
    }
    crate::shell_notify();
    if ok {
        0
    } else {
        1
    }
}

fn run(args: String) -> Result<()> {
    logfile::write(&format!("requesting elevation: {args}"));
    match shell::run_elevated(&args)? {
        0 => Ok(()),
        code => Err(Error::Invalid(format!("The admin helper reported a problem (code {code}); see log.txt"))),
    }
}

/// Step 3 of the known-good repair for these types (one UAC prompt for all of them).
pub fn fix_machine(items: &[KnownGood]) -> Result<()> {
    if items.is_empty() {
        return Ok(());
    }
    if shell::is_elevated() {
        let reg = WinRegistry;
        return items.iter().try_for_each(|k| repair::fix_machine(&reg, *k));
    }
    run(std::iter::once("--elevated-fix".to_owned()).chain(items.iter().map(|k| k.ext.to_owned())).collect::<Vec<_>>().join(" "))
}

/// Restores HKLM class defaults captured in a backup (Undo of a repair).
pub fn validate_restore(ops: &[Op]) -> Result<()> {
    restore_pairs(ops).map(|_| ())
}

fn restore_pairs(ops: &[Op]) -> Result<Vec<String>> {
    let mut pairs = Vec::new();
    for op in ops {
        let Op::SetValue(Hive::LocalMachine, path, name, value) = op else {
            return Err(Error::Forbidden(format!("{op:?}")));
        };
        let ext = path.rsplit('\\').next().unwrap_or_default();
        if !name.is_empty() || repair::known(ext).is_none() || !path.eq_ignore_ascii_case(&format!("SOFTWARE\\Classes\\{ext}")) {
            return Err(Error::Forbidden(path.clone()));
        }
        pairs.push(match value {
            Some(Value::Sz(v)) if plain_prog_id(v) => format!("{ext}={v}"),
            None => format!("{ext}=-"),
            Some(_) => return Err(Error::Invalid(format!("Cannot restore the original value type or data for {ext}; no settings were restored"))),
        });
    }
    Ok(pairs)
}

pub fn restore_machine(ops: &[Op]) -> Result<()> {
    let pairs = restore_pairs(ops)?;
    if pairs.is_empty() {
        return Ok(());
    }
    if shell::is_elevated() {
        return match main(&std::iter::once("--elevated-set".to_owned()).chain(pairs).collect::<Vec<_>>()) {
            0 => Ok(()),
            _ => Err(Error::Invalid("Could not restore the system-wide value; see log.txt".into())),
        };
    }
    run(format!("--elevated-set {}", pairs.join(" ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(value: Option<Value>) -> Op {
        Op::SetValue(Hive::LocalMachine, "SOFTWARE\\Classes\\.bat".into(), String::new(), value)
    }

    #[test]
    fn unsupported_machine_values_are_not_treated_as_absent() {
        for value in [Value::Dword(42), Value::Raw(3, vec![1, 2]), Value::ExpandSz("batfile".into()), Value::Sz(String::new())] {
            assert!(validate_restore(&[op(Some(value))]).is_err());
        }
        assert_eq!(restore_pairs(&[op(None)]).unwrap(), [".bat=-"]);
        assert_eq!(restore_pairs(&[op(Some(Value::Sz("batfile".into())))]).unwrap(), [".bat=batfile"]);
    }

    #[test]
    fn machine_restore_validates_every_operation_before_running() {
        assert!(restore_pairs(&[op(Some(Value::Sz("batfile".into()))), op(Some(Value::Dword(1)))]).is_err());
        assert!(validate_restore(&[Op::DeleteTree(Hive::LocalMachine, "SOFTWARE\\Classes\\.bat".into())]).is_err());
    }
}
