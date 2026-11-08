use anyhow::{anyhow, bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

/// Where a module source's files are in the recovery: the vendor_boot
/// ramdisk's modules under /lib/modules, the vendor_dlkm and vendor
/// partitions where they are mounted, and the kernel's own modules in
/// system_dlkm, which it also keeps in one folder.
pub fn source_dir(source: &str) -> Result<PathBuf> {
    Ok(PathBuf::from(match source {
        "vendor_boot" => "/lib/modules",
        "vendor_dlkm" => "/vendor_dlkm/lib/modules",
        "vendor" => "/vendor/lib/modules",
        "system_dlkm" => "/system_dlkm/flatten/lib/modules",
        _ => bail!("unknown module source {source:?}"),
    }))
}

/// Load `names` (module names without `.ko`) with their dependencies. Each
/// comes from the first of `dirs` that holds it, and its dependencies from
/// whichever directory holds them: a phone keeps one driver in the vendor
/// partition and the parts of the kernel it is built on in the ramdisk, so a
/// single directory holds neither the whole of a driver nor everything it
/// needs. A module the kernel already has counts as loaded. Returns the files
/// loaded, dependencies first.
pub fn load(dirs: &[PathBuf], names: &[String]) -> Result<Vec<PathBuf>> {
    let mut loaded = Vec::new();
    let mut done = BTreeSet::new();
    for name in names {
        let file = format!("{name}.ko");
        let dir = dirs
            .iter()
            .find(|d| d.join(&file).is_file())
            .ok_or_else(|| anyhow!("{file} is in none of {dirs:?}"))?;
        let graph = parse_dep(dir)?;
        for m in graph.load_order(std::slice::from_ref(&file)) {
            if !done.insert(m.clone()) {
                continue;
            }
            // The graph knows where the module it named lists its own parts,
            // which is not always where this one came from.
            let path = graph.resolve_path(&m).or_else(|| elsewhere(dirs, &m));
            let path = path.with_context(|| format!("{m} is listed in {}/modules.dep but is in no source", dir.display()))?;
            insmod(&path, "")?;
            loaded.push(path);
        }
    }
    Ok(loaded)
}

/// A module file in any of `dirs`, for one a directory's own graph names but
/// does not hold.
fn elsewhere(dirs: &[PathBuf], file: &str) -> Option<PathBuf> {
    dirs.iter().map(|d| d.join(file)).find(|p| p.is_file())
}

/// A `modules.dep` graph: each module file mapped to the module files it
/// depends on. Keys and values are reduced to bare file names so the graph is
/// independent of where the modules were sourced from (GKI devices keep them
/// in several places: vendor_boot ramdisk, vendor_dlkm, vendor).
#[derive(Debug, Default)]
pub struct ModuleGraph {
    pub base: PathBuf,
    pub deps: BTreeMap<String, Vec<String>>,
}

pub fn parse_dep(base: impl AsRef<Path>) -> Result<ModuleGraph> {
    let base = base.as_ref().to_path_buf();
    let dep_path = base.join("modules.dep");
    let text = fs::read_to_string(&dep_path)
        .with_context(|| format!("read {}", dep_path.display()))?;
    let mut deps = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((lhs, rhs)) = line.split_once(':') else {
            continue;
        };
        let name = basename(lhs.trim());
        let ds = rhs.split_whitespace().map(basename).collect();
        deps.insert(name, ds);
    }
    Ok(ModuleGraph { base, deps })
}

fn basename(p: &str) -> String {
    Path::new(p)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.to_string())
}

impl ModuleGraph {
    /// Topologically ordered load list. With an empty `requested`, every module
    /// in the graph is scheduled; otherwise only the requested modules and
    /// their transitive dependencies, dependencies first.
    pub fn load_order(&self, requested: &[String]) -> Vec<String> {
        let mut order = Vec::new();
        let mut seen = BTreeSet::new();
        let list: Vec<String> = if requested.is_empty() {
            self.deps.keys().cloned().collect()
        } else {
            requested.to_vec()
        };
        for m in list {
            self.visit(&m, &mut seen, &mut order);
        }
        order
    }

    fn visit(&self, m: &str, seen: &mut BTreeSet<String>, order: &mut Vec<String>) {
        if !seen.insert(m.to_string()) {
            return;
        }
        if let Some(ds) = self.deps.get(m) {
            for d in ds {
                self.visit(d, seen, order);
            }
        }
        order.push(m.to_string());
    }

    pub fn resolve_path(&self, module: &str) -> Option<PathBuf> {
        let direct = self.base.join(module);
        if direct.exists() {
            return Some(direct);
        }
        find_file(&self.base, module)
    }
}

fn find_file(dir: &Path, name: &str) -> Option<PathBuf> {
    let rd = fs::read_dir(dir).ok()?;
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            if let Some(f) = find_file(&p, name) {
                return Some(f);
            }
        } else if p.file_name().map(|s| s == name).unwrap_or(false) {
            return Some(p);
        }
    }
    None
}

#[cfg(target_os = "android")]
pub fn insmod(path: impl AsRef<Path>, params: &str) -> Result<()> {
    use std::ffi::CString;
    use std::os::unix::io::AsRawFd;
    let path = path.as_ref();
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let cparams = CString::new(params)?;
    // The phone's modules in vendor_dlkm are built with a different compiler
    // stamp than the recovery kernel -- 6.6.57 where the kernel is 6.6.118 --
    // and this kernel has no CONFIG_MODULE_FORCE_LOAD, so the
    // MODULE_INIT_IGNORE_* flags mean nothing to it. The phone's own init
    // loads them all the same, and /proc/modules marks them (O).
    //
    // init_module is the call that does load them: the toybox insmod on this
    // phone uses it, and loading the touchscreen's drivers through it
    // succeeded where finit_module answered ENOEXEC. finit_module stays as the
    // fallback for a module it might take and this one refuse.
    let rc = unsafe { libc::syscall(libc::SYS_init_module, bytes.as_ptr(), bytes.len(), cparams.as_ptr()) };
    if rc == 0 {
        return Ok(());
    }
    let first = std::io::Error::last_os_error();
    if first.raw_os_error() == Some(libc::EEXIST) {
        return Ok(());
    }
    let file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let rc = unsafe { libc::syscall(libc::SYS_finit_module, file.as_raw_fd(), cparams.as_ptr(), 0) };
    if rc != 0 {
        let second = std::io::Error::last_os_error();
        if second.raw_os_error() == Some(libc::EEXIST) {
            return Ok(());
        }
        return Err(anyhow::Error::new(second).context(format!(
            "load {} (init_module said {first}, finit_module said otherwise)",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(not(target_os = "android"))]
pub fn insmod(_path: impl AsRef<Path>, _params: &str) -> Result<()> {
    anyhow::bail!("insmod is only implemented on android")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orders_dependencies_first() {
        let mut deps = BTreeMap::new();
        deps.insert("a.ko".to_string(), vec!["b.ko".to_string()]);
        deps.insert("b.ko".to_string(), vec!["c.ko".to_string()]);
        deps.insert("c.ko".to_string(), vec![]);
        let g = ModuleGraph { base: PathBuf::new(), deps };
        let order = g.load_order(&["a.ko".to_string()]);
        assert_eq!(order, vec!["c.ko", "b.ko", "a.ko"]);
    }

    #[test]
    fn strips_paths_to_basenames() {
        assert_eq!(basename("kernel/drivers/foo/bar.ko"), "bar.ko");
    }
}
