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

use std::error::Error;
use std::ffi::OsString;
use std::fmt::{self, Display, Formatter};

use super::helpers::RedoPathBuf;

/// The error type for the `redo` crate.
#[derive(Debug)]
pub struct RedoError {
    pub(crate) kind: RedoErrorKind,
    pub(crate) msg: String,
}

impl RedoError {
    /// Returns a generic error with the given message.
    #[inline]
    pub fn new(msg: String) -> RedoError {
        RedoError {
            kind: RedoErrorKind::default(),
            msg,
        }
    }

    #[inline]
    pub(crate) fn wrap_generic<E>(e: E) -> RedoError
    where
        E: Error + Send + Sync + 'static,
    {
        RedoError {
            kind: RedoErrorKind::Generic,
            msg: format!("redo: {}", e),
        }
    }

    #[inline]
    pub fn kind(&self) -> &RedoErrorKind {
        &self.kind
    }
}

impl Display for RedoError {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        Display::fmt(&self.msg, f)
    }
}

impl Error for RedoError {}

impl From<RedoErrorKind> for RedoError {
    fn from(kind: RedoErrorKind) -> RedoError {
        RedoError {
            msg: format!("{}", kind),
            kind,
        }
    }
}

#[derive(Clone, Eq, PartialEq, Debug)]
#[non_exhaustive]
pub enum RedoErrorKind {
    Generic,
    FailedInAnotherThread { target: RedoPathBuf },
    InvalidTarget(OsString),
    CyclicDependency,
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
        }
    }
}
