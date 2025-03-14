use std::collections::{HashMap, HashSet};
use std::convert::TryFrom;
use std::iter::FromIterator;
use std::path::{Path, PathBuf};

use bstr::ByteVec;
use eyre::Context;
use itertools::Itertools;
use tracing::{instrument, warn};

use super::oid::make_non_zero_oid;
use super::status::FileMode;
use super::{MaybeZeroOid, NonZeroOid, Repo};

pub struct TreeEntry<'repo> {
    pub(super) inner: git2::TreeEntry<'repo>,
}

impl TreeEntry<'_> {
    /// Get the object ID for this tree entry.
    pub fn get_oid(&self) -> NonZeroOid {
        make_non_zero_oid(self.inner.id())
    }

    /// Get the object filemode for this tree entry.
    pub fn get_filemode(&self) -> FileMode {
        FileMode::from(self.inner.filemode())
    }
}

/// A tree object. Contains a mapping from name to OID.
#[derive(Debug)]
pub struct Tree<'repo> {
    pub(super) inner: git2::Tree<'repo>,
}

impl Tree<'_> {
    /// Get the object ID for this tree.
    pub fn get_oid(&self) -> NonZeroOid {
        make_non_zero_oid(self.inner.id())
    }

    /// Determine whether this tree is empty (i.e. contains no entries).
    ///
    /// This doesn't happen in typical practice, since the index can't represent
    /// empty directories (trees). However, this can happen when operating on
    /// trees directly.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Get the tree entry for the the given path.
    ///
    /// Note that the path isn't just restricted to entries of the current tree,
    /// i.e. you can use slashes in the provided path.
    pub fn get_path(&self, path: &Path) -> eyre::Result<Option<TreeEntry>> {
        match self.inner.get_path(path) {
            Ok(entry) => Ok(Some(TreeEntry { inner: entry })),
            Err(err) if err.code() == git2::ErrorCode::NotFound => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    /// Get the OID for the entry with the given path.
    ///
    /// Note that the path isn't just restricted to entries of the current tree,
    /// i.e. you can use slashes in the provided path.
    pub fn get_oid_for_path(&self, path: &Path) -> eyre::Result<Option<MaybeZeroOid>> {
        self.get_path(path)
            .map(|maybe_entry| maybe_entry.map(|entry| entry.inner.id().into()))
    }
}

/// This function is a hot code path. Do not annotate with `#[instrument]`, and
/// be mindful of performance/memory allocations.
fn get_changed_paths_between_trees_internal(
    repo: &Repo,
    acc: &mut Vec<Vec<PathBuf>>,
    current_path: &[PathBuf],
    lhs: Option<&git2::Tree>,
    rhs: Option<&git2::Tree>,
) -> eyre::Result<()> {
    let lhs_entries = lhs
        .map(|tree| tree.iter().collect_vec())
        .unwrap_or_default();
    let lhs_entries: HashMap<&[u8], &git2::TreeEntry> = lhs_entries
        .iter()
        .map(|entry| (entry.name_bytes(), entry))
        .collect();

    let rhs_entries = rhs
        .map(|tree| tree.iter().collect_vec())
        .unwrap_or_default();
    let rhs_entries: HashMap<&[u8], &git2::TreeEntry> = rhs_entries
        .iter()
        .map(|entry| (entry.name_bytes(), entry))
        .collect();

    let all_entry_names: HashSet<&[u8]> = lhs_entries
        .keys()
        .chain(rhs_entries.keys())
        .cloned()
        .collect();
    let entries: HashMap<&[u8], (Option<&git2::TreeEntry>, Option<&git2::TreeEntry>)> =
        all_entry_names
            .into_iter()
            .map(|entry_name| {
                (
                    entry_name,
                    (
                        lhs_entries.get(entry_name).copied(),
                        rhs_entries.get(entry_name).copied(),
                    ),
                )
            })
            .collect();

    for (entry_name, (lhs_entry, rhs_entry)) in entries {
        enum ClassifiedEntry {
            Absent,
            NotATree(git2::Oid, i32),
            Tree(git2::Oid, i32),
        }

        fn classify_entry(entry: Option<&git2::TreeEntry>) -> eyre::Result<ClassifiedEntry> {
            let entry = match entry {
                Some(entry) => entry,
                None => return Ok(ClassifiedEntry::Absent),
            };

            let file_mode = entry.filemode_raw();
            match entry.kind() {
                Some(git2::ObjectType::Tree) => Ok(ClassifiedEntry::Tree(entry.id(), file_mode)),
                _ => Ok(ClassifiedEntry::NotATree(entry.id(), file_mode)),
            }
        }

        let get_tree = |oid| {
            let entry_oid = MaybeZeroOid::from(oid);
            let entry_oid = NonZeroOid::try_from(entry_oid)?;
            let entry_tree = repo.find_tree(entry_oid)?;
            entry_tree.ok_or_else(|| {
                eyre::eyre!(
                    "Tree entry was said to be an object of kind tree, \
                            but it could not be looked up: {:?}",
                    oid
                )
            })
        };

        let full_entry_path = || -> eyre::Result<Vec<PathBuf>> {
            let mut full_entry_path = current_path.to_vec();
            full_entry_path.push(entry_name.to_vec().into_path_buf()?);
            Ok(full_entry_path)
        };
        match (classify_entry(lhs_entry)?, classify_entry(rhs_entry)?) {
            (ClassifiedEntry::Absent, ClassifiedEntry::Absent) => {
                // Shouldn't happen, but there's no issue here.
            }

            (
                ClassifiedEntry::NotATree(lhs_oid, lhs_file_mode),
                ClassifiedEntry::NotATree(rhs_oid, rhs_file_mode),
            ) => {
                if lhs_oid == rhs_oid && lhs_file_mode == rhs_file_mode {
                    // Unchanged file, do nothing.
                } else {
                    // Changed file.
                    acc.push(full_entry_path()?);
                }
            }

            (ClassifiedEntry::Absent, ClassifiedEntry::NotATree(_, _))
            | (ClassifiedEntry::NotATree(_, _), ClassifiedEntry::Absent) => {
                // Added, removed, or changed file.
                acc.push(full_entry_path()?);
            }

            (ClassifiedEntry::Absent, ClassifiedEntry::Tree(tree_oid, _))
            | (ClassifiedEntry::Tree(tree_oid, _), ClassifiedEntry::Absent) => {
                // A directory was added or removed. Add all entries from that
                // directory.
                let full_entry_path = full_entry_path()?;
                let tree = get_tree(tree_oid)?;
                get_changed_paths_between_trees_internal(
                    repo,
                    acc,
                    &full_entry_path,
                    Some(&tree.inner),
                    None,
                )?;
            }

            (ClassifiedEntry::NotATree(_, _), ClassifiedEntry::Tree(tree_oid, _))
            | (ClassifiedEntry::Tree(tree_oid, _), ClassifiedEntry::NotATree(_, _)) => {
                // A file was changed into a directory. Add both the file and
                // all subdirectory entries as changed entries.
                let full_entry_path = full_entry_path()?;
                let tree = get_tree(tree_oid)?;
                get_changed_paths_between_trees_internal(
                    repo,
                    acc,
                    &full_entry_path,
                    Some(&tree.inner),
                    None,
                )?;
                acc.push(full_entry_path);
            }

            (
                ClassifiedEntry::Tree(lhs_tree_oid, lhs_file_mode),
                ClassifiedEntry::Tree(rhs_tree_oid, rhs_file_mode),
            ) => {
                match (
                    (lhs_tree_oid == rhs_tree_oid),
                    // Note that there should only be one possible file mode for
                    // an entry which points to a tree, but it's possible that
                    // some extra non-meaningful bits are set. Should we report
                    // a change in that case? This code takes the conservative
                    // approach and reports a change.
                    (lhs_file_mode == rhs_file_mode),
                ) {
                    (true, true) => {
                        // Unchanged entry, do nothing.
                    }

                    (true, false) => {
                        // Only the directory changed, but none of its contents.
                        acc.push(full_entry_path()?);
                    }

                    (false, true) => {
                        let lhs_tree = get_tree(lhs_tree_oid)?;
                        let rhs_tree = get_tree(rhs_tree_oid)?;

                        // Only include the files changed in the subtrees, and
                        // not the directory itself.
                        get_changed_paths_between_trees_internal(
                            repo,
                            acc,
                            &full_entry_path()?,
                            Some(&lhs_tree.inner),
                            Some(&rhs_tree.inner),
                        )?;
                    }

                    (false, false) => {
                        let lhs_tree = get_tree(lhs_tree_oid)?;
                        let rhs_tree = get_tree(rhs_tree_oid)?;
                        let full_entry_path = full_entry_path()?;

                        get_changed_paths_between_trees_internal(
                            repo,
                            acc,
                            &full_entry_path,
                            Some(&lhs_tree.inner),
                            Some(&rhs_tree.inner),
                        )?;
                        acc.push(full_entry_path);
                    }
                }
            }
        }
    }

    Ok(())
}

#[instrument]
pub fn get_changed_paths_between_trees(
    repo: &Repo,
    lhs: Option<&git2::Tree>,
    rhs: Option<&git2::Tree>,
) -> eyre::Result<HashSet<PathBuf>> {
    let mut acc = Vec::new();
    get_changed_paths_between_trees_internal(repo, &mut acc, &Vec::new(), lhs, rhs)?;
    let changed_paths: HashSet<PathBuf> = acc.into_iter().map(PathBuf::from_iter).collect();
    Ok(changed_paths)
}

/// Add the provided entries into the tree.
///
/// If the provided `Tree` is `None`, then this function adds the entries to the
/// empty tree.
///
/// The paths for the provided entries can contain slashes.
///
/// If the value for an entry is `None`, then that element in the tree is
/// removed. If a directory ever becomes empty, then it's removed from its
/// parent directory.
///
/// If a path for a given entry is already present in the provided tree, then
/// that entry is overwritten.
///
/// If a path refers to intermediate directories that don't exist in the
/// provided tree, then those intermediate directories are created.
#[instrument]
pub fn hydrate_tree(
    repo: &Repo,
    tree: Option<&Tree>,
    entries: HashMap<PathBuf, Option<(NonZeroOid, FileMode)>>,
) -> eyre::Result<NonZeroOid> {
    let (file_entries, dir_entries) = {
        let mut file_entries: HashMap<PathBuf, Option<(NonZeroOid, FileMode)>> = HashMap::new();
        let mut dir_entries: HashMap<PathBuf, HashMap<PathBuf, Option<(NonZeroOid, FileMode)>>> =
            HashMap::new();
        for (path, value) in entries {
            match path.components().collect_vec().as_slice() {
                [] => {
                    warn!(?tree, ?value, "Empty path when hydrating tree");
                }
                [file_name] => {
                    file_entries.insert(file_name.into(), value);
                }
                components => {
                    let first: PathBuf = [components[0]].iter().collect();
                    let rest: PathBuf = components[1..].iter().collect();
                    dir_entries.entry(first).or_default().insert(rest, value);
                }
            }
        }
        (file_entries, dir_entries)
    };

    let tree = tree.map(|tree| &tree.inner);
    let mut builder = repo
        .inner
        .treebuilder(tree)
        .wrap_err("Instantiating tree builder")?;
    for (file_name, file_value) in file_entries {
        match file_value {
            Some((oid, file_mode)) => {
                builder
                    .insert(&file_name, oid.inner, file_mode.into())
                    .wrap_err_with(|| {
                        format!(
                            "Inserting file {:?} with OID: {:?}, file mode: {:?}",
                            &file_name, oid, file_mode
                        )
                    })?;
            }
            None => {
                remove_entry_if_exists(&mut builder, &file_name)
                    .wrap_err_with(|| format!("Removing deleted file: {:?}", &file_name))?;
            }
        }
    }

    for (dir_name, dir_value) in dir_entries {
        let existing_dir_entry: Option<Tree> = match builder.get(&dir_name)? {
            Some(existing_dir_entry)
                if !existing_dir_entry.id().is_zero()
                    && existing_dir_entry.kind() == Some(git2::ObjectType::Tree) =>
            {
                repo.find_tree(make_non_zero_oid(existing_dir_entry.id()))?
            }
            _ => None,
        };
        let new_entry_oid = hydrate_tree(repo, existing_dir_entry.as_ref(), dir_value)?;

        let new_entry_tree = repo
            .find_tree(new_entry_oid)?
            .ok_or_else(|| eyre::eyre!("Could not find just-hydrated tree: {:?}", new_entry_oid))?;
        if new_entry_tree.is_empty() {
            remove_entry_if_exists(&mut builder, &dir_name)
                .wrap_err_with(|| format!("Removing empty directory: {:?}", &dir_name))?;
        } else {
            builder
                .insert(&dir_name, new_entry_oid.inner, git2::FileMode::Tree.into())
                .wrap_err_with(|| {
                    format!(
                        "Inserting directory {:?} with OID: {:?}",
                        &dir_name, new_entry_oid
                    )
                })?;
        }
    }

    let tree_oid = builder.write().wrap_err("Building tree")?;
    Ok(make_non_zero_oid(tree_oid))
}

pub fn make_empty_tree(repo: &Repo) -> eyre::Result<Tree> {
    let tree_oid = hydrate_tree(repo, None, Default::default())?;
    repo.find_tree_or_fail(tree_oid)
}

/// `libgit2` raises an error if the entry isn't present, but that's often not
/// an error condition here. We may be referring to a created or deleted path,
/// which wouldn't exist in one of the pre-/post-patch trees.
fn remove_entry_if_exists(builder: &mut git2::TreeBuilder, name: &Path) -> eyre::Result<()> {
    if builder.get(&name)?.is_some() {
        builder.remove(&name)?;
    }
    Ok(())
}

/// Filter the entries in the provided tree by only keeping the provided paths.
///
/// If a provided path does not appear in the tree at all, then it's ignored.
#[instrument]
pub fn dehydrate_tree(repo: &Repo, tree: &Tree, paths: &[&Path]) -> eyre::Result<NonZeroOid> {
    let entries: HashMap<PathBuf, Option<(NonZeroOid, FileMode)>> = paths
        .iter()
        .map(|path| -> eyre::Result<(PathBuf, _)> {
            let key = path.to_path_buf();
            match tree.inner.get_path(path) {
                Ok(tree_entry) => {
                    let value = Some((
                        make_non_zero_oid(tree_entry.id()),
                        FileMode::from(tree_entry.filemode()),
                    ));
                    Ok((key, value))
                }
                Err(err) if err.code() == git2::ErrorCode::NotFound => Ok((key, None)),
                Err(err) => Err(err.into()),
            }
        })
        .try_collect()?;

    hydrate_tree(repo, None, entries)
}

#[cfg(test)]
mod tests {
    use std::convert::TryInto;

    use super::*;

    use crate::testing::make_git;

    fn dump_tree_entries(tree: &Tree) -> String {
        tree.inner
            .iter()
            .map(|entry| format!("{:?} {:?}\n", entry.name().unwrap(), entry.id()))
            .collect()
    }

    #[test]
    fn test_hydrate_tree() -> eyre::Result<()> {
        let git = make_git()?;

        git.init_repo()?;

        git.write_file("foo", "foo")?;
        git.write_file("bar/bar", "bar")?;
        git.write_file("bar/baz", "qux")?;
        git.write_file("xyzzy", "xyzzy")?;
        git.run(&["add", "."])?;
        git.run(&["commit", "-m", "commit"])?;

        let repo = git.get_repo()?;
        let head_oid = repo.get_head_info()?.oid.unwrap();
        let head_commit = repo.find_commit_or_fail(head_oid)?;
        let head_tree = head_commit.get_tree()?;

        insta::assert_snapshot!(dump_tree_entries(&head_tree), @r###"
        "bar" 778e23a1e80b1feb10e00b15b29a33315929c5b5
        "foo.txt" 19102815663d23f8b75a47e7a01965dcdc96468c
        "initial.txt" 63af22885f8665a312ba8b83db722134f1f8290d
        "xyzzy.txt" 7c465afc533f95ff7d2c91e18921f94aac8292fc
        "###);

        {
            let hydrated_tree = {
                let hydrated_tree_oid = hydrate_tree(&repo, Some(&head_tree), {
                    let mut result = HashMap::new();
                    result.insert(
                        PathBuf::from("foo-copy.txt"),
                        Some((
                            head_tree
                                .get_oid_for_path(&PathBuf::from("foo.txt"))?
                                .unwrap()
                                .try_into()?,
                            FileMode::from(0o100644),
                        )),
                    );
                    result.insert(PathBuf::from("foo.txt"), None);
                    result
                })?;
                repo.find_tree(hydrated_tree_oid)?.unwrap()
            };
            insta::assert_snapshot!(dump_tree_entries(&hydrated_tree), @r###"
            "bar" 778e23a1e80b1feb10e00b15b29a33315929c5b5
            "foo-copy.txt" 19102815663d23f8b75a47e7a01965dcdc96468c
            "initial.txt" 63af22885f8665a312ba8b83db722134f1f8290d
            "xyzzy.txt" 7c465afc533f95ff7d2c91e18921f94aac8292fc
            "###);
        }

        {
            let hydrated_tree = {
                let hydrated_tree_oid = hydrate_tree(&repo, Some(&head_tree), {
                    let mut result = HashMap::new();
                    result.insert(PathBuf::from("bar/bar.txt"), None);
                    result
                })?;
                repo.find_tree(hydrated_tree_oid)?.unwrap()
            };
            insta::assert_snapshot!(dump_tree_entries(&hydrated_tree), @r###"
            "bar" 08ee88e1c53fbd01ab76f136a4f2c9d759b981d0
            "foo.txt" 19102815663d23f8b75a47e7a01965dcdc96468c
            "initial.txt" 63af22885f8665a312ba8b83db722134f1f8290d
            "xyzzy.txt" 7c465afc533f95ff7d2c91e18921f94aac8292fc
            "###);
        }

        {
            let hydrated_tree = {
                let hydrated_tree_oid = hydrate_tree(&repo, Some(&head_tree), {
                    let mut result = HashMap::new();
                    result.insert(PathBuf::from("bar/bar.txt"), None);
                    result.insert(PathBuf::from("bar/baz.txt"), None);
                    result
                })?;
                repo.find_tree(hydrated_tree_oid)?.unwrap()
            };
            insta::assert_snapshot!(dump_tree_entries(&hydrated_tree), @r###"
            "foo.txt" 19102815663d23f8b75a47e7a01965dcdc96468c
            "initial.txt" 63af22885f8665a312ba8b83db722134f1f8290d
            "xyzzy.txt" 7c465afc533f95ff7d2c91e18921f94aac8292fc
            "###);
        }

        {
            let dehydrated_tree_oid = dehydrate_tree(
                &repo,
                &head_tree,
                &[Path::new("bar/baz.txt"), Path::new("foo.txt")],
            )?;
            let dehydrated_tree = repo.find_tree(dehydrated_tree_oid)?.unwrap();
            insta::assert_snapshot!(dump_tree_entries(&dehydrated_tree), @r###"
            "bar" 08ee88e1c53fbd01ab76f136a4f2c9d759b981d0
            "foo.txt" 19102815663d23f8b75a47e7a01965dcdc96468c
            "###);
        }

        Ok(())
    }

    #[test]
    fn test_detect_path_only_changed_file_mode() -> eyre::Result<()> {
        let git = make_git()?;
        git.init_repo()?;

        git.run(&["update-index", "--chmod=+x", "initial.txt"])?;
        git.run(&["commit", "-m", "update file mode"])?;

        let repo = git.get_repo()?;
        let oid = repo.get_head_info()?.oid.unwrap();
        let commit = repo.find_commit_or_fail(oid)?;

        let lhs = commit.get_only_parent().unwrap();
        let lhs_tree = lhs.get_tree()?;
        let rhs_tree = commit.get_tree()?;
        let changed_paths =
            get_changed_paths_between_trees(&repo, Some(&lhs_tree.inner), Some(&rhs_tree.inner))?;

        insta::assert_debug_snapshot!(changed_paths, @r###"
        {
            "initial.txt",
        }
        "###);

        Ok(())
    }
}
