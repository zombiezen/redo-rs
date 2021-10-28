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

//! Code for detecting and aborting on cyclic dependency loops.

use std::borrow::Cow;
use std::collections::HashSet;
use std::env;

use super::builder::{BuildError, BuildErrorKind};

const CYCLES_VAR: &str = "REDO_CYCLES";

/// Get the set of held cycle items.
fn get() -> HashSet<String> {
    use std::iter::FromIterator;

    env::var(CYCLES_VAR)
        .map(|v| HashSet::from_iter(v.split(':').map(|s| s.to_string())))
        .unwrap_or_default()
}

/// Add a lock to the list of held cycle items.
pub(crate) fn add<'a, S: Into<Cow<'a, str>>>(fid: S) {
    use std::iter::FromIterator;

    let fid = fid.into();
    let mut items = get();
    if !items.contains(&fid as &str) {
        items.insert(fid.into_owned());
        let items = Vec::from_iter(items);
        env::set_var(CYCLES_VAR, items.join(":"));
    }
}

pub(crate) fn check<S: AsRef<str>>(fid: S) -> Result<(), BuildError> {
    if get().contains(fid.as_ref()) {
        // Lock already held by parent: cyclic dependency
        Err(BuildErrorKind::CyclicDependency.into())
    } else {
        Ok(())
    }
}
