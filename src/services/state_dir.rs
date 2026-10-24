//! The `.symora` directory a project keeps symora's state in.

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Everything symora writes into `.symora` rebuilds from source — the index,
/// its caches and their locks — while the configuration and ignore rules a
/// project keeps there are its own to commit.
const GITIGNORE: &str = "\
# Written by symora: the index, caches and locks here rebuild from source.
*
!config.toml
!ignore
";

/// The project's `.symora`, created with the `.gitignore` that keeps the
/// state symora writes there out of the project's history, so committing the
/// whole tree never takes the index with it. A `.gitignore` already there is
/// the project's own and stays as it is.
pub(crate) fn create(project_root: &Path) -> io::Result<PathBuf> {
    let dir = project_root.join(".symora");
    std::fs::create_dir_all(&dir)?;
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dir.join(".gitignore"))
    {
        Ok(mut file) => file.write_all(GITIGNORE.as_bytes())?,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_state_directory_ignores_what_symora_writes_and_keeps_a_projects_own_rules() {
        let root = tempfile::tempdir().unwrap();
        let dir = create(root.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join(".gitignore")).unwrap(),
            GITIGNORE
        );

        std::fs::write(dir.join(".gitignore"), "store.db\n").unwrap();
        create(root.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join(".gitignore")).unwrap(),
            "store.db\n"
        );
    }
}
