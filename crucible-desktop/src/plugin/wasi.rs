//! The WASI preview 1 calls a plugin may import, answered without giving it anything: no files,
//! arguments or environment, a clock that only counts its own readings, and the same "random"
//! bytes every time. Its standard input is what the host chose to hand it, and its standard
//! output and error go back to the host.
//! Toolchains that cannot build a module with no imports, such as Go's, need them to start.

use wasmi::{Caller, Error, Extern, Linker, Memory, errors::LinkerError};

pub const MODULE: &str = "wasi_snapshot_preview1";

const NAMES: [&str; 17] = [
    "args_get",
    "args_sizes_get",
    "clock_time_get",
    "environ_get",
    "environ_sizes_get",
    "fd_close",
    "fd_fdstat_get",
    "fd_fdstat_set_flags",
    "fd_prestat_dir_name",
    "fd_prestat_get",
    "fd_read",
    "fd_seek",
    "fd_write",
    "poll_oneoff",
    "proc_exit",
    "random_get",
    "sched_yield",
];

const OK: i32 = 0;
const BAD_DESCRIPTOR: i32 = 8;
const FAULT: i32 = 21;
const NOT_SEEKABLE: i32 = 70;

/// What one call keeps for its plugin.
pub trait Guest {
    /// Takes what the plugin wrote to descriptor `fd`: 1 is its standard output, anything else
    /// its standard error.
    fn write(&mut self, fd: i32, bytes: &[u8]);

    /// Fills `buffer` from the plugin's standard input and returns how much it filled; zero at
    /// the end.
    fn read(&mut self, buffer: &mut [u8]) -> usize;

    /// The clock's next reading in nanoseconds: later than the last, and never zero.
    fn tick(&mut self) -> u64;
}

/// Whether the host answers the WASI call `name`.
pub fn offers(name: &str) -> bool {
    NAMES.contains(&name)
}

fn memory<T>(caller: &Caller<'_, T>) -> Option<Memory> {
    match caller.get_export("memory")? {
        Extern::Memory(memory) => Some(memory),
        _ => None,
    }
}

fn write<T>(caller: &mut Caller<'_, T>, at: i32, bytes: &[u8]) -> i32 {
    let Some(memory) = memory(caller) else {
        return FAULT;
    };
    match memory.write(caller, at as u32 as usize, bytes) {
        Ok(()) => OK,
        Err(_) => FAULT,
    }
}

fn read<T, const N: usize>(caller: &Caller<'_, T>, at: u32) -> Option<[u8; N]> {
    let mut bytes = [0; N];
    memory(caller)?.read(caller, at as usize, &mut bytes).ok()?;
    Some(bytes)
}

/// Zero for each of two counts.
fn none<T>(mut caller: Caller<'_, T>, count: i32, size: i32) -> i32 {
    match write(&mut caller, count, &[0; 4]) {
        OK => write(&mut caller, size, &[0; 4]),
        error => error,
    }
}

/// The most of one buffer the host takes from a plugin in one write, or hands it in one read.
const SAID: usize = 1 << 20;

/// Hands the `count` buffers listed at `list` to the plugin's [`Guest`], and reports them all
/// written.
fn fd_write<T: Guest>(
    mut caller: Caller<'_, T>,
    fd: i32,
    list: i32,
    count: i32,
    written: i32,
) -> i32 {
    let mut total: u32 = 0;
    for i in 0..count as u32 {
        let entry = (list as u32).wrapping_add(i.wrapping_mul(8));
        let Some(pair) = read::<T, 8>(&caller, entry) else {
            return FAULT;
        };
        let at = u32::from_le_bytes([pair[0], pair[1], pair[2], pair[3]]);
        let len = u32::from_le_bytes([pair[4], pair[5], pair[6], pair[7]]);
        let mut text = vec![0; (len as usize).min(SAID)];
        let Some(memory) = memory(&caller) else {
            return FAULT;
        };
        if memory.read(&caller, at as usize, &mut text).is_err() {
            return FAULT;
        }
        caller.data_mut().write(fd, &text);
        total = total.wrapping_add(len);
    }
    write(&mut caller, written, &total.to_le_bytes())
}

/// Fills the `count` buffers listed at `list` from the plugin's standard input, stopping at the
/// first it cannot fill.
fn fd_read<T: Guest>(mut caller: Caller<'_, T>, fd: i32, list: i32, count: i32, got: i32) -> i32 {
    if fd != 0 {
        return BAD_DESCRIPTOR;
    }
    let mut total: u32 = 0;
    for i in 0..count as u32 {
        let entry = (list as u32).wrapping_add(i.wrapping_mul(8));
        let Some(pair) = read::<T, 8>(&caller, entry) else {
            return FAULT;
        };
        let at = u32::from_le_bytes([pair[0], pair[1], pair[2], pair[3]]);
        let len = u32::from_le_bytes([pair[4], pair[5], pair[6], pair[7]]);
        let mut buffer = vec![0; (len as usize).min(SAID)];
        let filled = caller.data_mut().read(&mut buffer);
        if write(&mut caller, at as i32, &buffer[..filled]) != OK {
            return FAULT;
        }
        total = total.wrapping_add(filled as u32);
        if filled < len as usize {
            break;
        }
    }
    write(&mut caller, got, &total.to_le_bytes())
}

/// Every one of `count` subscriptions at `subs` reported at `events` as having happened.
fn poll_oneoff<T>(
    mut caller: Caller<'_, T>,
    subs: i32,
    events: i32,
    count: i32,
    fired: i32,
) -> i32 {
    for i in 0..count as u32 {
        let sub = (subs as u32).wrapping_add(i.wrapping_mul(48));
        let Some(head) = read::<T, 9>(&caller, sub) else {
            return FAULT;
        };
        let mut event = [0; 32];
        event[..8].copy_from_slice(&head[..8]);
        event[10] = head[8];
        let at = (events as u32).wrapping_add(i.wrapping_mul(32));
        if write(&mut caller, at as i32, &event) != OK {
            return FAULT;
        }
    }
    write(&mut caller, fired, &count.to_le_bytes())
}

fn random_get<T>(mut caller: Caller<'_, T>, at: i32, len: i32) -> i32 {
    const CHUNK: [u8; 64] = [0x5a; 64];
    let mut done: u32 = 0;
    while done < len as u32 {
        let step = (len as u32 - done).min(CHUNK.len() as u32);
        let to = (at as u32).wrapping_add(done) as i32;
        if write(&mut caller, to, &CHUNK[..step as usize]) != OK {
            return FAULT;
        }
        done += step;
    }
    OK
}

pub fn link<T: Guest + 'static>(linker: &mut Linker<T>) -> Result<(), LinkerError> {
    linker
        .func_wrap(MODULE, "args_get", |_: i32, _: i32| OK)?
        .func_wrap(MODULE, "args_sizes_get", none::<T>)?
        .func_wrap(
            MODULE,
            "clock_time_get",
            |mut caller: Caller<'_, T>, _clock: i32, _precision: i64, time: i32| {
                let now = caller.data_mut().tick();
                write(&mut caller, time, &now.to_le_bytes())
            },
        )?
        .func_wrap(MODULE, "environ_get", |_: i32, _: i32| OK)?
        .func_wrap(MODULE, "environ_sizes_get", none::<T>)?
        .func_wrap(MODULE, "fd_close", |_: i32| BAD_DESCRIPTOR)?
        .func_wrap(MODULE, "fd_fdstat_get", |_: i32, _: i32| BAD_DESCRIPTOR)?
        .func_wrap(MODULE, "fd_fdstat_set_flags", |_: i32, _: i32| {
            BAD_DESCRIPTOR
        })?
        .func_wrap(MODULE, "fd_prestat_dir_name", |_: i32, _: i32, _: i32| {
            BAD_DESCRIPTOR
        })?
        .func_wrap(MODULE, "fd_prestat_get", |_: i32, _: i32| BAD_DESCRIPTOR)?
        .func_wrap(MODULE, "fd_read", fd_read::<T>)?
        .func_wrap(MODULE, "fd_seek", |_: i32, _: i64, _: i32, _: i32| {
            NOT_SEEKABLE
        })?
        .func_wrap(MODULE, "fd_write", fd_write::<T>)?
        .func_wrap(MODULE, "poll_oneoff", poll_oneoff::<T>)?
        .func_wrap(MODULE, "proc_exit", |code: i32| -> Result<(), Error> {
            Err(Error::i32_exit(code))
        })?
        .func_wrap(MODULE, "random_get", random_get::<T>)?
        .func_wrap(MODULE, "sched_yield", || OK)?;
    Ok(())
}
