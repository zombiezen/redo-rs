// Copyright 2021 Ross Light
// Copyright 2010-2018 Avery Pennarun and contributors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
// SPDX-License-Identifier: Apache-2.0

use failure::Error;
use ouroboros::self_referencing;
use std::ffi::{OsStr, OsString};
use std::iter::FusedIterator;
use std::mem;
use std::path::{Path, PathBuf};
use std::str::MatchIndices;

use super::helpers;
use super::state::{self, DepMode, ProcessTransaction};

/// An iterator over the default.do patterns for a given file name.
#[derive(Clone, Debug)]
struct DefaultDoFiles<'a> {
    filename: &'a str,
    l: Option<MatchIndices<'a, char>>,
}

impl<'a> From<&'a str> for DefaultDoFiles<'a> {
    fn from(filename: &'a str) -> DefaultDoFiles<'a> {
        DefaultDoFiles {
            filename,
            l: Some(filename.match_indices('.')),
        }
    }
}

impl<'a> Iterator for DefaultDoFiles<'a> {
    type Item = (String, &'a str, &'a str);

    fn next(&mut self) -> Option<Self::Item> {
        let maybe_match = match self.l.as_mut() {
            Some(l) => l.next(),
            None => return None,
        };
        match maybe_match {
            Some((i, _)) => {
                let basename = &self.filename[..i];
                let ext = &self.filename[i..];
                Some((format!("default{}.do", ext), basename, ext))
            }
            None => {
                // Last iteration of loop: yield default.do.
                self.l = None;
                Some((String::from("default.do"), self.filename, ""))
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        match &self.l {
            None => (0, Some(0)),
            Some(l) => {
                let (lower, upper) = l.size_hint();
                (lower, upper.map(|u| u + 1))
            }
        }
    }
}

impl<'a> FusedIterator for DefaultDoFiles<'a> {}

/// Information about a single .do file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DoFile {
    /// Absolute path to the directory that the .do file is located in.
    pub(crate) do_dir: PathBuf,
    /// Name of the .do file.
    pub(crate) do_file: OsString,
    /// Path of the directory that the target file is located in,
    /// relative to the `do_dir`.
    pub(crate) base_dir: PathBuf,
    /// Path of the target file, relative to the `do_dir`, with `ext` stripped.
    pub(crate) base_name: PathBuf,
    /// Extension used for default.do matching
    /// (i.e. `ext = ".c"` implies `do_file = "default.c.do"`).
    pub(crate) ext: OsString,
}

impl DoFile {
    /// Returns the absolute path to the directory that the .do file is located in.
    pub fn do_dir(&self) -> &Path {
        &self.do_dir
    }

    /// Returns the name of the .do file.
    pub fn do_file(&self) -> &OsStr {
        &self.do_file
    }
}

/// Iterator over the list of .do files needed to build a given path,
/// returned by [`possible_do_files`].
#[derive(Debug)]
pub struct PossibleDoFiles {
    state: DoFilesState,
}

#[derive(Debug)]
enum DoFilesState {
    First(PathBuf),
    Recursive(RecursiveDoFilesState),
    Stopped,
}

/// Create an iterator over the .do files for the absolute path `p`.
///
/// # Panics
///
/// If `p` is not absolute.
pub fn possible_do_files<P: AsRef<Path>>(p: P) -> PossibleDoFiles {
    assert!(p.as_ref().is_absolute());
    PossibleDoFiles {
        state: DoFilesState::First(helpers::normpath(p.as_ref()).to_path_buf()),
    }
}

impl Iterator for PossibleDoFiles {
    type Item = DoFile;

    fn next(&mut self) -> Option<DoFile> {
        let mut state = DoFilesState::Stopped;
        mem::swap(&mut state, &mut self.state);
        match state {
            DoFilesState::First(t) => {
                let result = {
                    let dirname = t.parent().unwrap_or(&t);
                    let filename = t.file_name().unwrap();
                    let mut do_file = filename.to_os_string();
                    do_file.push(".do");
                    Some(DoFile {
                        do_dir: PathBuf::from(dirname),
                        do_file,
                        base_dir: PathBuf::new(),
                        base_name: filename.into(),
                        ext: OsString::new(),
                    })
                };
                self.state = DoFilesState::Recursive(RecursiveDoFilesState::from_path_buf(t));
                result
            }
            DoFilesState::Recursive(mut state) => match state.next() {
                result @ Some(_) => {
                    self.state = DoFilesState::Recursive(state);
                    result
                }
                None => {
                    self.state = DoFilesState::Stopped;
                    None
                }
            },
            DoFilesState::Stopped => None,
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        match &self.state {
            DoFilesState::First(_) => (1, None),
            DoFilesState::Recursive(state) => state.size_hint(),
            DoFilesState::Stopped => (0, Some(0)),
        }
    }
}

impl FusedIterator for PossibleDoFiles {}

#[self_referencing]
#[derive(Debug)]
struct RecursiveDoFilesState {
    norm_path: PathBuf,
    #[borrows(norm_path)]
    #[covariant]
    dir_bits: Vec<(&'this Path, &'this Path)>, // stored in reverse order as stack
    #[borrows(norm_path)]
    #[not_covariant]
    default_do_files: DefaultDoFiles<'this>,
}

impl RecursiveDoFilesState {
    fn from_path_buf(norm_path: PathBuf) -> RecursiveDoFilesState {
        use std::iter::FromIterator;
        RecursiveDoFilesStateBuilder {
            norm_path,
            dir_bits_builder: |norm_path| {
                norm_path
                    .parent()
                    .map(|dirname| {
                        let mut bits = path_splits(dirname);
                        bits.reverse();
                        Vec::from_iter(bits.into_iter())
                    })
                    .unwrap_or_default()
            },
            default_do_files_builder: |norm_path| {
                DefaultDoFiles::from(
                    norm_path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap(),
                )
            },
        }
        .build()
    }
}

impl Iterator for RecursiveDoFilesState {
    type Item = DoFile;

    fn next(&mut self) -> Option<DoFile> {
        // It's important to try every possibility in a directory before resorting
        // to a parent directory.  Think about nested projects: We don't want
        // ../../default.o.do to take precedence over ../default.do, because
        // the former one might just be an artifact of someone embedding my project
        // into theirs as a subdir.  When they do, my rules should still be used
        // for building my project in *all* cases.
        self.with_mut(|fields| loop {
            let (base_dir, sub_dir) = match fields.dir_bits.last().copied() {
                Some(bit) => bit,
                None => return None,
            };
            let next_name = fields.default_do_files.next();
            if let Some((do_file, base_name, ext)) = next_name {
                return Some(DoFile {
                    do_dir: base_dir.to_path_buf(),
                    do_file: do_file.into(),
                    base_dir: sub_dir.to_path_buf(),
                    base_name: sub_dir.join(base_name),
                    ext: ext.into(),
                });
            }
            fields.dir_bits.pop();
            *fields.default_do_files = DefaultDoFiles::from(
                fields
                    .norm_path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap(),
            );
        })
    }
}

pub(crate) fn find_do_file(
    ptx: &mut ProcessTransaction,
    f: &mut state::File,
) -> Result<Option<DoFile>, Error> {
    for do_file in possible_do_files(helpers::abs_path(ptx.state().env().base(), f.name())) {
        let do_path = do_file.do_dir.join(&do_file.do_file);
        log_debug2!(
            "{}: {}:{} ?\n",
            f.name(),
            do_file.do_dir.to_str().unwrap(),
            do_file.do_file.to_str().unwrap()
        );
        if do_path.exists() {
            f.add_dep(ptx, DepMode::Modified, &do_path)?;
            return Ok(Some(do_file));
        } else {
            f.add_dep(ptx, DepMode::Created, &do_path)?;
        }
    }
    Ok(None)
}

fn path_splits<'a, P: AsRef<Path> + ?Sized>(p: &'a P) -> Vec<(&'a Path, &'a Path)> {
    let p = p.as_ref();
    let subs = {
        let mut subs = Vec::new();
        let mut it = p.iter();
        loop {
            let sub = it.as_path();
            if it.next().is_none() {
                break;
            }
            subs.push(sub);
        }
        subs.push(Path::new(""));
        subs
    };
    let bases = p.ancestors();
    bases.zip(subs.into_iter().rev()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_do_files_empty_string() {
        let got: Vec<(String, &str, &str)> = DefaultDoFiles::from("").collect();
        assert_eq!(got, vec![(String::from("default.do"), "", "")]);
    }

    #[test]
    fn default_do_files_no_dots() {
        let got: Vec<(String, &str, &str)> = DefaultDoFiles::from("foo").collect();
        assert_eq!(got, vec![(String::from("default.do"), "foo", "")]);
    }

    #[test]
    fn default_do_files_with_dots() {
        let got: Vec<(String, &str, &str)> = DefaultDoFiles::from("foo.gen.c").collect();
        assert_eq!(
            got,
            vec![
                (String::from("default.gen.c.do"), "foo", ".gen.c"),
                (String::from("default.c.do"), "foo.gen", ".c"),
                (String::from("default.do"), "foo.gen.c", ""),
            ]
        );
    }

    #[test]
    fn possible_do_files_test() {
        use std::iter::FromIterator;

        assert_eq!(
            Vec::from_iter(possible_do_files(PathBuf::from(
                "/src/redo-rs/bin/redo-log.o"
            ))),
            vec![
                DoFile {
                    do_dir: "/src/redo-rs/bin".into(),
                    do_file: "redo-log.o.do".into(),
                    base_dir: "".into(),
                    base_name: "redo-log.o".into(),
                    ext: "".into(),
                },
                DoFile {
                    do_dir: "/src/redo-rs/bin".into(),
                    do_file: "default.o.do".into(),
                    base_dir: "".into(),
                    base_name: "redo-log".into(),
                    ext: ".o".into(),
                },
                DoFile {
                    do_dir: "/src/redo-rs/bin".into(),
                    do_file: "default.do".into(),
                    base_dir: "".into(),
                    base_name: "redo-log.o".into(),
                    ext: "".into(),
                },
                DoFile {
                    do_dir: "/src/redo-rs".into(),
                    do_file: "default.o.do".into(),
                    base_dir: "bin".into(),
                    base_name: "bin/redo-log".into(),
                    ext: ".o".into(),
                },
                DoFile {
                    do_dir: "/src/redo-rs".into(),
                    do_file: "default.do".into(),
                    base_dir: "bin".into(),
                    base_name: "bin/redo-log.o".into(),
                    ext: "".into(),
                },
                DoFile {
                    do_dir: "/src".into(),
                    do_file: "default.o.do".into(),
                    base_dir: "redo-rs/bin".into(),
                    base_name: "redo-rs/bin/redo-log".into(),
                    ext: ".o".into(),
                },
                DoFile {
                    do_dir: "/src".into(),
                    do_file: "default.do".into(),
                    base_dir: "redo-rs/bin".into(),
                    base_name: "redo-rs/bin/redo-log.o".into(),
                    ext: "".into(),
                },
                DoFile {
                    do_dir: "/".into(),
                    do_file: "default.o.do".into(),
                    base_dir: "src/redo-rs/bin".into(),
                    base_name: "src/redo-rs/bin/redo-log".into(),
                    ext: ".o".into(),
                },
                DoFile {
                    do_dir: "/".into(),
                    do_file: "default.do".into(),
                    base_dir: "src/redo-rs/bin".into(),
                    base_name: "src/redo-rs/bin/redo-log.o".into(),
                    ext: "".into(),
                },
            ]
        )
    }

    #[test]
    fn path_splits_test() {
        assert_eq!(
            path_splits(Path::new("foo")),
            vec![
                (Path::new("foo"), Path::new("")),
                (Path::new(""), Path::new("foo")),
            ]
        );
        assert_eq!(
            path_splits(Path::new("/foo")),
            vec![
                (Path::new("/foo"), Path::new("")),
                (Path::new("/"), Path::new("foo")),
            ]
        );
        assert_eq!(
            path_splits(Path::new("foo/bar")),
            vec![
                (Path::new("foo/bar"), Path::new("")),
                (Path::new("foo"), Path::new("bar")),
                (Path::new(""), Path::new("foo/bar")),
            ]
        );
        assert_eq!(
            path_splits(Path::new("foo/bar/baz")),
            vec![
                (Path::new("foo/bar/baz"), Path::new("")),
                (Path::new("foo/bar"), Path::new("baz")),
                (Path::new("foo"), Path::new("bar/baz")),
                (Path::new(""), Path::new("foo/bar/baz")),
            ]
        );
    }
}
