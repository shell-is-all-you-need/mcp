use std::{
    ffi::OsString,
    path::{Component, Path, PathBuf},
};

#[derive(Clone)]
pub(super) struct PathPolicy {
    roots: Vec<PathBuf>,
    denied: Vec<PathBuf>,
    cwd: PathBuf,
}

fn lexical_absolute(path: &Path, cwd: &Path) -> Result<PathBuf, String> {
    let source = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let mut output = PathBuf::new();
    for component in source.components() {
        match component {
            Component::Prefix(value) => output.push(value.as_os_str()),
            Component::RootDir => output.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !output.pop() {
                    return Err(format!(
                        "path escapes the filesystem namespace: {}",
                        path.display()
                    ));
                }
            }
            Component::Normal(value) => output.push(value),
        }
    }
    Ok(output)
}

fn resolve(path: &Path, cwd: &Path) -> Result<PathBuf, String> {
    let absolute = lexical_absolute(path, cwd)?;
    if absolute.exists() {
        return std::fs::canonicalize(&absolute)
            .map_err(|error| format!("cannot resolve {}: {error}", path.display()));
    }

    let mut ancestor = absolute.as_path();
    let mut suffix = Vec::<OsString>::new();
    while !ancestor.exists() {
        let name = ancestor
            .file_name()
            .ok_or_else(|| format!("cannot resolve {}", path.display()))?;
        suffix.push(name.to_os_string());
        ancestor = ancestor
            .parent()
            .ok_or_else(|| format!("cannot resolve {}", path.display()))?;
    }

    let mut resolved = std::fs::canonicalize(ancestor)
        .map_err(|error| format!("cannot resolve {}: {error}", ancestor.display()))?;
    for part in suffix.iter().rev() {
        resolved.push(part);
    }
    Ok(resolved)
}

fn contains(root: &Path, target: &Path) -> bool {
    target == root || target.starts_with(root)
}

impl PathPolicy {
    pub(super) fn new(roots: Vec<String>, denied: Vec<String>) -> Result<Self, String> {
        let cwd = std::fs::canonicalize(
            std::env::current_dir()
                .map_err(|error| format!("cannot read current directory: {error}"))?,
        )
        .map_err(|error| format!("cannot resolve current directory: {error}"))?;

        let roots = if roots.is_empty() {
            vec![".".into()]
        } else {
            roots
        };
        let roots = roots
            .into_iter()
            .map(|path| {
                let resolved = resolve(Path::new(&path), &cwd)
                    .map_err(|error| format!("cannot resolve filesystem root {path:?}: {error}"))?;
                if !resolved.is_dir() {
                    return Err(format!("filesystem root is not a directory: {path}"));
                }
                Ok(resolved)
            })
            .collect::<Result<Vec<_>, _>>()?;

        let denied = denied
            .into_iter()
            .map(|path| resolve(Path::new(&path), &cwd))
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Self { roots, denied, cwd })
    }

    pub(super) fn check(&self, field: &str, value: &str) -> Result<(), String> {
        let target = resolve(Path::new(value), &self.cwd)?;
        if self.denied.iter().any(|path| contains(path, &target)) {
            return Err(format!(
                "path rendered from field {field:?} is denied: {}",
                target.display()
            ));
        }
        if !self.roots.iter().any(|path| contains(path, &target)) {
            return Err(format!(
                "path rendered from field {field:?} is outside configured filesystem roots: {}",
                target.display()
            ));
        }
        Ok(())
    }
}
