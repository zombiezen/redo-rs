// Copyright 2021 Ross Light
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

use std::borrow::Cow;
use std::error::Error;
use std::ffi::OsString;
use std::fmt::{self, Display, Formatter};

use super::helpers::RedoPathBuf;

/// The primary error type for the `redo` crate.
#[derive(Debug)]
pub struct RedoError {
    kind: RedoErrorKind,
    msg: Cow<'static, str>,
    cause: Option<Box<dyn Error + Send + Sync + 'static>>,
}

impl RedoError {
    /// Returns a generic error with the given message.
    #[inline]
    pub fn new<S>(msg: S) -> RedoError
    where
        S: Into<Cow<'static, str>>,
    {
        RedoError {
            kind: RedoErrorKind::default(),
            msg: msg.into(),
            cause: None,
        }
    }

    /// Returns a generic error that contains another error as its message.
    /// The error is not presented on the source chain.
    #[inline]
    pub(crate) fn opaque_error<E: Display>(e: E) -> RedoError {
        RedoError {
            kind: RedoErrorKind::default(),
            msg: Cow::Owned(format!("redo: {}", e)),
            cause: None,
        }
    }

    /// Returns a `RedoError` that wraps an underlying error.
    #[inline]
    pub(crate) fn wrap<E, S>(cause: E, msg: S) -> RedoError
    where
        E: Error + Send + Sync + 'static,
        S: Into<Cow<'static, str>>,
    {
        RedoError {
            kind: RedoErrorKind::default(),
            msg: msg.into(),
            cause: Some(Box::new(cause)),
        }
    }

    #[inline]
    pub fn kind(&self) -> &RedoErrorKind {
        &self.kind
    }

    #[inline]
    pub(crate) fn with_kind(self, kind: RedoErrorKind) -> RedoError {
        RedoError { kind, ..self }
    }
}

impl Display for RedoError {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        Display::fmt(&self.msg, f)
    }
}

impl Error for RedoError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.cause.as_ref().map(|cause| &**cause as &dyn Error)
    }
}

impl From<RedoErrorKind> for RedoError {
    fn from(kind: RedoErrorKind) -> RedoError {
        RedoError {
            msg: Cow::Owned(kind.to_string()),
            kind,
            cause: None,
        }
    }
}

/// Classification of a [`RedoError`].
#[derive(Clone, Eq, PartialEq, Debug)]
#[non_exhaustive]
pub enum RedoErrorKind {
    Generic,
    FailedInAnotherThread { target: RedoPathBuf },
    InvalidTarget(OsString),
    CyclicDependency,
    FileNotFound,
}

impl RedoErrorKind {
    /// Returns the first non-[`RedoErrorKind::Generic`] error kind in the error chain or
    /// [`RedoErrorKind::Generic`]
    pub fn of<E: Error + 'static>(e: &E) -> &RedoErrorKind {
        let mut next: Option<&(dyn Error + 'static)> = Some(e);
        while let Some(e) = next {
            next = e.source();
            let kind = match e.downcast_ref::<RedoError>() {
                Some(e) => e.kind(),
                None => continue,
            };
            if kind != &RedoErrorKind::Generic {
                return kind;
            }
        }
        &RedoErrorKind::Generic
    }
}

impl Default for RedoErrorKind {
    #[inline]
    fn default() -> RedoErrorKind {
        RedoErrorKind::Generic
    }
}

impl Display for RedoErrorKind {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        match self {
            RedoErrorKind::Generic => f.write_str("error"),
            RedoErrorKind::FailedInAnotherThread { target } => {
                write!(f, "{:?}: failed in another thread", target)
            }
            RedoErrorKind::InvalidTarget(target) => write!(f, "invalid target {:?}", target),
            RedoErrorKind::CyclicDependency => f.write_str("cyclic dependency detected"),
            RedoErrorKind::FileNotFound => f.write_str("file not found"),
        }
    }
}
