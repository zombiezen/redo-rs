use failure::{format_err, Error};
use std::borrow::Cow;
use std::ffi::{OsStr, OsString};
use std::iter::FusedIterator;
use std::mem;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::str::MatchIndices;

use super::env::Env;
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

#[derive(Clone, Debug)]
pub(crate) struct DoFile {
    do_dir: PathBuf,
    do_file: OsString,
    base_dir: Cow<'static, Path>,
    base_name: PathBuf,
    ext: Cow<'static, OsStr>,
}

/// Iterator over the list of .do files needed to build a given path.
#[derive(Debug)]
pub(crate) struct DoFiles<'a, 'e> {
    state: DoFilesState<'a>,
    base: &'e Path,
}

#[derive(Debug)]
enum DoFilesState<'a> {
    First(&'a Path),
    Recursive(RecursiveDoFilesState),
    Stopped,
}

impl<'a, 'e> DoFiles<'a, 'e> {
    pub(crate) fn new<P: AsRef<Path>>(v: &'e Env, p: &'a P) -> DoFiles<'a, 'e> {
        DoFiles {
            state: DoFilesState::First(p.as_ref()),
            base: &v.base,
        }
    }
}

impl<'a, 'e> Iterator for DoFiles<'a, 'e> {
    type Item = Result<DoFile, Error>;

    fn next(&mut self) -> Option<Result<DoFile, Error>> {
        match self.state {
            DoFilesState::First(t) => {
                let result = {
                    let dirname = t.parent().unwrap_or(t);
                    let filename = t.file_name().unwrap();
                    let mut do_file = filename.to_os_string();
                    do_file.push(".do");
                    Some(Ok(DoFile {
                        do_dir: self.base.join(dirname),
                        do_file,
                        base_dir: Cow::Borrowed("".as_ref()),
                        base_name: filename.into(),
                        ext: Cow::Borrowed("".as_ref()),
                    }))
                };
                let norm_t = helpers::normpath(&self.base.join(t)).into_owned();
                self.state = DoFilesState::Recursive(RecursiveDoFilesState::new(norm_t));
                result
            }
            DoFilesState::Recursive(ref mut state) => match state.next() {
                result @ Some(_) => result,
                None => {
                    self.state = DoFilesState::Stopped;
                    None
                }
            }
            DoFilesState::Stopped => None,
        }
    }
}

#[derive(Debug)]
struct RecursiveDoFilesState {
    norm_path: Pin<Box<PathBuf>>,
    dir_bits: Vec<(*const Path, *const Path)>, // stored in reverse order as stack
    default_do_files: DefaultDoFiles<'static>, // refers to memory in norm_path
}

impl RecursiveDoFilesState {
    fn new(norm_path: PathBuf) -> RecursiveDoFilesState {
        let norm_path = Pin::new(Box::new(norm_path));
        let dir_bits: Vec<(*const Path, *const Path)> = {
            let dir_bits = norm_path.parent().map(|dirname| {
                let mut bits = path_splits(dirname);
                bits.reverse();
                bits
            }).unwrap_or_default();
            unsafe { mem::transmute(dir_bits) }
        };
        let default_do_files = unsafe {
            RecursiveDoFilesState::default_do_files_for(&norm_path.as_ref())
        };
        RecursiveDoFilesState {
            norm_path,
            dir_bits,
            default_do_files,
        }
    }

    unsafe fn default_do_files_for(norm_path: &Pin<&PathBuf>) -> DefaultDoFiles<'static> {
        // TODO(soon): Trigger error instead of panic if file_name() is non-UTF-8.
        mem::transmute(DefaultDoFiles::from(norm_path.file_name().and_then(|name| name.to_str()).unwrap()))
    }
}

impl Iterator for RecursiveDoFilesState {
    type Item = Result<DoFile, Error>;

    fn next(&mut self) -> Option<Result<DoFile, Error>> {
        // It's important to try every possibility in a directory before resorting
        // to a parent directory.  Think about nested projects: We don't want
        // ../../default.o.do to take precedence over ../default.do, because
        // the former one might just be an artifact of someone embedding my project
        // into theirs as a subdir.  When they do, my rules should still be used
        // for building my project in *all* cases.
        loop {
            let (base_dir, sub_dir) = match self.dir_bits.last().copied().map(|(base, sub)| unsafe { (&*base, &*sub) }) {
                Some(bit) => bit,
                None => return None,
            };
            let next_name = self.default_do_files.next();
            if let Some((do_file, base_name, ext)) = next_name {
                return Some(Ok(DoFile {
                    do_dir: base_dir.to_path_buf(),
                    do_file: do_file.into(),
                    base_dir: Cow::Owned(base_dir.to_path_buf()),
                    base_name: sub_dir.join(base_name),
                    ext: Cow::Owned(ext.into()),
                }));
            }
            self.dir_bits.pop();
            self.default_do_files = unsafe {
                RecursiveDoFilesState::default_do_files_for(&self.norm_path.as_ref())
            };
        }
    }
}

pub(crate) fn find_do_file(
    ptx: &mut ProcessTransaction,
    f: &mut state::File,
) -> Result<Option<DoFile>, Error> {
    let mut created_paths: Vec<PathBuf> = Vec::new();
    for result in DoFiles::new(ptx.state().env(), &f.name) {
        let do_file = result?;
        let do_path = do_file.do_dir.join(&do_file.do_file);
        if do_path.exists() {
            for p in created_paths {
                f.add_dep(ptx, DepMode::Created, os_str_as_str(&p)?)?;
            }
            f.add_dep(ptx, DepMode::Modified, os_str_as_str(&do_path)?)?;
            return Ok(Some(do_file));
        }
        created_paths.push(do_path);
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
        subs
    };
    let bases = p.ancestors().skip(1);
    bases.zip(subs.into_iter().rev()).collect()
}

fn os_str_as_str<'a, S: AsRef<OsStr>>(s: &'a S) -> Result<&'a str, Error> {
    let s = s.as_ref();
    s.to_str()
        .ok_or_else(|| format_err!("path {:?} is invalid UTF-8", s))
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
    fn path_splits_test() {
        assert_eq!(path_splits(Path::new("foo.txt")), vec![
            (Path::new(""), Path::new("foo.txt")),
        ]);
        assert_eq!(path_splits(Path::new("/foo.txt")), vec![
            (Path::new("/"), Path::new("foo.txt")),
        ]);
        assert_eq!(path_splits(Path::new("foo/bar.txt")), vec![
            (Path::new("foo"), Path::new("bar.txt")),
            (Path::new(""), Path::new("foo/bar.txt")),
        ]);
        assert_eq!(path_splits(Path::new("foo/bar/baz.txt")), vec![
            (Path::new("foo/bar"), Path::new("baz.txt")),
            (Path::new("foo"), Path::new("bar/baz.txt")),
            (Path::new(""), Path::new("foo/bar/baz.txt")),
        ]);
    }
}
