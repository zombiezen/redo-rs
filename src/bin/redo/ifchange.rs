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

use rusqlite::TransactionBehavior;
use std::io;
use std::path::PathBuf;
use thiserror::Error;

use redo::builder::{self, StdinLogReader, StdinLogReaderBuilder};
use redo::logs::LogBuilder;
use redo::{
    self, log_debug2, log_err, DepMode, Dirtiness, Env, JobServer, ProcessState,
    ProcessTransaction, RedoError, RedoPath, RedoPathBuf,
};

pub(crate) fn run() -> Result<(), anyhow::Error> {
    use std::convert::TryFrom;

    let mut targets = {
        let mut targets = Vec::<RedoPathBuf>::new();
        for arg in std::env::args_os().skip(1) {
            targets.push(RedoPathBuf::try_from(arg)?);
        }
        targets
    };
    let env = Env::init(targets.as_slice())?;
    let mut ps = ProcessState::init(env)?;
    if ps.is_toplevel() && targets.is_empty() {
        targets.push(unsafe { RedoPathBuf::from_string_unchecked("all".into()) });
    }
    let mut _stdin_log_reader: Option<StdinLogReader> = None; // held during operation
    if ps.is_toplevel() && ps.env().log().unwrap_or(true) {
        builder::close_stdin()?;
        _stdin_log_reader = Some(StdinLogReaderBuilder::default().start(ps.env())?);
    } else {
        LogBuilder::from(ps.env()).setup(io::stderr());
    }

    let mut server;
    {
        let mut ptx = ProcessTransaction::new(&mut ps, TransactionBehavior::Immediate)?;
        let f = if !ptx.state().env().target().as_os_str().is_empty()
            && !ptx.state().env().is_unlocked()
        {
            let mut me = PathBuf::new();
            me.push(ptx.state().env().startdir());
            me.push(ptx.state().env().pwd());
            me.push(ptx.state().env().target());
            let f = redo::File::from_name(&mut ptx, &me, true)?;
            log_debug2!(
                "TARGET: {:?} {:?} {:?}\n",
                ptx.state().env().startdir(),
                ptx.state().env().pwd(),
                ptx.state().env().target()
            );
            Some(f)
        } else {
            log_debug2!("redo-ifchange: not adding depends.\n");
            None
        };
        server = JobServer::setup(0)?;
        if let Some(mut f) = f {
            for t in targets.iter() {
                f.add_dep(&mut ptx, DepMode::Modified, t)?;
            }
            f.save(&mut ptx)?;
            ptx.commit()?;
        }
    }

    let build_result = server.block_on(builder::run(
        &mut ps,
        &server.handle(),
        &targets,
        should_build,
    ));
    // TODO(someday): In the original, there's a state.rollback call.
    // Unclear what this is trying to do.
    assert!(ps.is_flushed());
    let return_tokens_result = server.force_return_tokens();
    if let Err(e) = &return_tokens_result {
        log_err!("unexpected error: {}", e);
    }
    build_result
        .map_err(|e| e.into())
        .and(return_tokens_result.map_err(Into::into))
}

fn should_build(
    ptx: &mut ProcessTransaction,
    t: &RedoPath,
) -> Result<(bool, Dirtiness), ShouldBuildError> {
    let mut f = redo::File::from_name(ptx, t, true)?;
    if f.is_failed(ptx.state().env()) {
        // TODO(soon): ImmediateReturn(EXIT_TARGET_FAILED)
        return Err(format!("target {} failed", t).into());
    }
    let dirty = match redo::is_dirty(ptx, &mut f, &mut Default::default())? {
        Dirtiness::NeedTargets(t) if t.len() == 1 && t[0].id() == f.id() => Dirtiness::Dirty,
        d => d,
    };
    Ok((f.is_generated(), dirty))
}

#[derive(Error, Debug)]
enum ShouldBuildError {
    #[error("{0}")]
    Message(String),
    #[error(transparent)]
    RedoError(#[from] RedoError),
}

impl From<String> for ShouldBuildError {
    fn from(s: String) -> ShouldBuildError {
        ShouldBuildError::Message(s)
    }
}
