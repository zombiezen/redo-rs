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

/// Success exit code.
pub const EXIT_SUCCESS: i32 = 0;

/// Generic failure exit code.
pub const EXIT_FAILURE: i32 = 1;

/// Target failed in a concurrent run.
pub const EXIT_FAILED_IN_ANOTHER_THREAD: i32 = 2;

/// Attempt to get information about a target not in the redo database.
pub const EXIT_UNKNOWN_TARGET: i32 = 24;

/// Attempt to build a target that has already failed during the same run.
pub const EXIT_TARGET_FAILED: i32 = 32;

/// Failed to start internal helper subprocess.
pub(crate) const EXIT_HELPER_FAILURE: i32 = 99;

/// `MAKEFLAGS` specified invalid file descriptors.
pub(crate) const EXIT_INVALID_JOBSERVER: i32 = 200;

/// Internal error while spawning a job.
pub(crate) const EXIT_JOB_FAILURE: i32 = 201;

/// Invalid name for a target.
pub const EXIT_INVALID_TARGET: i32 = 204;

/// `$1` updated directly.
pub const EXIT_TARGET_DIRECTLY_MODIFIED: i32 = 206;

/// Standard out written to and `$3` created.
pub const EXIT_MULTIPLE_OUTPUTS: i32 = 207;

/// Cyclic dependency detected.
pub const EXIT_CYCLIC_DEPENDENCY: i32 = 208;

/// `BuildJob` internal error.
pub const EXIT_BUILD_JOB_ERROR: i32 = 209;
