/*
 * Copyright © 2018, Steve Smith <tarkasteve@gmail.com>
 *
 * This program is free software: you can redistribute it and/or
 * modify it under the terms of the GNU General Public License version
 * 3 as published by the Free Software Foundation.
 *
 * This program is distributed in the hope that it will be useful, but
 * WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the GNU
 * General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * along with this program.  If not, see <https://www.gnu.org/licenses/>.
 */

//! Parallelise copying at the file level. This can improve speed on
//! modern NVME devices, but can bottleneck on larger files.

use crossbeam_channel as cbc;
use log::{debug, error, info};
use libfs::{copy_node, FileType};
use std::fs::remove_file;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

use crate::config::Config;
use crate::drivers::CopyDriver;
use crate::errors::{Result, XcpError};
use crate::feedback::{StatusUpdate, StatusUpdater};
use crate::operations::{CopyHandle, Operation, tree_walker};

// ********************************************************************** //

pub struct Driver {
    config: Arc<Config>,
}

impl Driver {
    pub fn new(config: Arc<Config>) -> Result<Self> {
        Ok(Self {
            config,
        })
    }
}

impl CopyDriver for Driver {
    fn copy_all(&self, sources: Vec<PathBuf>, dest: &Path, stats: Arc<dyn StatusUpdater>) -> Result<()> {
        let (work_tx, work_rx) = cbc::unbounded();

        // Thread which walks the file tree and sends jobs to the
        // workers. The worker tx channel is moved to the walker so it is
        // closed, which will cause the workers to shutdown on completion.
        let _walk_worker = {
            let sc = stats.clone();
            let d = dest.to_path_buf();
            let o = self.config.clone();
            thread::spawn(move || tree_walker(sources, &d, &o, work_tx, sc))
        };

        // Worker threads. Will consume work and then shutdown once the
        // queue is closed by the walker.
        let nworkers = self.config.num_workers();
        let mut joins = Vec::with_capacity(nworkers);
        for _ in 0..nworkers {
            let copy_worker = {
                let wrx = work_rx.clone();
                let sc = stats.clone();
                let conf = self.config.clone();
                thread::spawn(move || copy_worker(wrx, &conf, sc))
            };
            joins.push(copy_worker);
        }

        for handle in joins {
            handle.join()
                .map_err(|_| XcpError::CopyError("Error during copy operation".to_string()))??;
        }

        Ok(())
    }

    fn copy_single(&self, source: &Path, dest: &Path, stats: Arc<dyn StatusUpdater>) -> Result<()> {
        let ft = FileType::from(source.metadata()?.file_type());
        match ft {
            FileType::File => {
                debug!("[Single] Copy file {:?} to {:?}", source, dest);
                CopyHandle::new(source, dest, &self.config)?
                    .copy_file(&stats)?;
            }

            FileType::Symlink => {
                debug!("[Single] Symlink {:?} to {:?}", source, dest);
                let _r = symlink(&source, &dest);
            }

            FileType::Dir => {
                let msg = format!("Attempt to copy directory in single-file operation: {:?} to {:?}", source, dest);
                error!("{}", msg);
                return Err(XcpError::InvalidArguments(msg).into());
            }

            FileType::Socket | FileType::Char | FileType::Fifo => {
                debug!("[Single] Special file {:?} -> {:?}", source, dest);
                if dest.exists() {
                    if self.config.no_clobber {
                        return Err(XcpError::DestinationExists("Destination file exists and --no-clobber is set.", dest.to_path_buf()).into());
                    }
                    remove_file(dest)?;
                }
                copy_node(&source, &dest)?;
            }

            FileType::Block | FileType::Other => {
                error!("Unsupported filetype found: {:?} -> {:?}", source, ft);
                return Err(XcpError::UnknownFileType(source.to_path_buf()).into());
            }
        };

        Ok(())
    }
}

// ********************************************************************** //

fn copy_worker(work: cbc::Receiver<Operation>, config: &Arc<Config>, updates: Arc<dyn StatusUpdater>) -> Result<()> {
    debug!("Starting copy worker {:?}", thread::current().id());
    for op in work {
        debug!("Received operation {:?}", op);

        match op {
            Operation::Copy(from, to) => {
                info!("Worker[{:?}]: Copy {:?} -> {:?}", thread::current().id(), from, to);
                // copy_file() sends back its own updates, but we should
                // send back any errors as they may have occurred
                // before the copy started..
                let r = CopyHandle::new(&from, &to, config)
                    .and_then(|hdl| hdl.copy_file(&updates));
                if let Err(e) = r {
                    updates.send(StatusUpdate::Error(XcpError::CopyError(e.to_string())))?;
                    error!("Error copying: {:?} -> {:?}; aborting.", from, to);
                    return Err(e)
                }
            }

            Operation::Link(from, to) => {
                info!("Worker[{:?}]: Symlink {:?} -> {:?}", thread::current().id(), from, to);
                let _r = symlink(&from, &to);
            }

            Operation::Special(from, to) => {
                info!("Worker[{:?}]: Special file {:?} -> {:?}", thread::current().id(), from, to);
                if to.exists() {
                    if config.no_clobber {
                        return Err(XcpError::DestinationExists("Destination file exists and --no-clobber is set.", to).into());
                    }
                    remove_file(&to)?;
                }
                copy_node(&from, &to)?;
            }

        }
    }
    debug!("Copy worker {:?} shutting down", thread::current().id());
    Ok(())
}
