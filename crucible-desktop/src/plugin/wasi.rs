//! The WASI preview 1 calls a plugin may import, answered without giving it anything: no files
//! or environment, a clock that only counts its own readings, and the same "random" bytes every
//! time. Its standard input is what the host chose to hand it, and its standard output and
//! error go back to the host. These are the calls the JavaScript engine module needs to start.
//!
//! Each call charges fuel for the bytes it moves and refuses a list of more than [`BUFFERS`]
//! buffers.

use wasmi::{Caller, Error, Extern, Linker, Memory, TrapCode, errors::LinkerError};

pub const MODULE: &str = "wasi_snapshot_preview1";

const NAMES: [&str; 10] = [
    "clock_time_get",
    "environ_get",
    "environ_sizes_get",
    "fd_close",
    "fd_fdstat_get",
    "fd_read",
    "fd_seek",
    "fd_write",
    "proc_exit",
    "random_get",
];

const OK: i32 = 0;
const BAD_DESCRIPTOR: i32 = 8;
const FAULT: i32 = 21;
const INVALID: i32 = 28;
const NOT_SEEKABLE: i32 = 70;

/// The most buffers one read or write may list.
const BUFFERS: u32 = 1024;
/// The fuel a host call costs before the bytes it moves, and each listed buffer on top.
const CALL: u64 = 100;
const BUFFER: u64 = 16;

/// What one call keeps for its plugin.
pub trait Guest {
    /// How many more bytes descriptor `fd` keeps: 1 is the plugin's standard output, anything
    /// else its standard error.
    fn room(&self, fd: i32) -> usize;

    /// Takes `bytes`, as much as there was room for of a write of `whole` bytes to `fd`.
    fn write(&mut self, fd: i32, bytes: &[u8], whole: usize);

    /// How much of the plugin's standard input is left to read.
    fn unread(&self) -> usize;

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

/// Takes `fuel` from the call, trapping as the interpreter does when there is not enough.
fn charge<T>(caller: &mut Caller<'_, T>, fuel: u64) -> Result<(), Error> {
    match caller.get_fuel()?.checked_sub(fuel) {
        Some(left) => caller.set_fuel(left),
        None => {
            caller.set_fuel(0)?;
            Err(Error::from(TrapCode::OutOfFuel))
        }
    }
}

fn memory<T>(caller: &Caller<'_, T>) -> Option<Memory> {
    match caller.get_export("memory")? {
        Extern::Memory(memory) => Some(memory),
        _ => None,
    }
}

fn write<T>(caller: &mut Caller<'_, T>, at: u32, bytes: &[u8]) -> i32 {
    let Some(memory) = memory(caller) else {
        return FAULT;
    };
    match memory.write(caller, at as usize, bytes) {
        Ok(()) => OK,
        Err(_) => FAULT,
    }
}

/// The address and length of buffer `i` of the list at `list`.
fn buffer<T>(caller: &Caller<'_, T>, list: u32, i: u32) -> Option<(u32, u32)> {
    let mut pair = [0; 8];
    let entry = list.wrapping_add(i.wrapping_mul(8));
    memory(caller)?
        .read(caller, entry as usize, &mut pair)
        .ok()?;
    Some((
        u32::from_le_bytes([pair[0], pair[1], pair[2], pair[3]]),
        u32::from_le_bytes([pair[4], pair[5], pair[6], pair[7]]),
    ))
}

/// Zero for each of two counts.
fn none<T>(mut caller: Caller<'_, T>, count: i32, size: i32) -> Result<i32, Error> {
    charge(&mut caller, CALL)?;
    Ok(match write(&mut caller, count as u32, &[0; 4]) {
        OK => write(&mut caller, size as u32, &[0; 4]),
        error => error,
    })
}

/// Hands the `count` buffers listed at `list` to the plugin's [`Guest`], as far as it has
/// room, and reports them all written.
fn fd_write<T: Guest>(
    mut caller: Caller<'_, T>,
    fd: i32,
    list: i32,
    count: i32,
    written: i32,
) -> Result<i32, Error> {
    let count = count as u32;
    if count > BUFFERS {
        return Ok(INVALID);
    }
    charge(&mut caller, CALL + BUFFER * u64::from(count))?;
    let mut total: u32 = 0;
    for i in 0..count {
        let Some((at, len)) = buffer(&caller, list as u32, i) else {
            return Ok(FAULT);
        };
        let take = (len as usize).min(caller.data().room(fd));
        charge(&mut caller, take as u64)?;
        let mut bytes = vec![0; take];
        let Some(memory) = memory(&caller) else {
            return Ok(FAULT);
        };
        if memory.read(&caller, at as usize, &mut bytes).is_err() {
            return Ok(FAULT);
        }
        caller.data_mut().write(fd, &bytes, len as usize);
        total = total.wrapping_add(len);
    }
    Ok(write(&mut caller, written as u32, &total.to_le_bytes()))
}

/// Fills the `count` buffers listed at `list` from the plugin's standard input, stopping at the
/// first it cannot fill.
fn fd_read<T: Guest>(
    mut caller: Caller<'_, T>,
    fd: i32,
    list: i32,
    count: i32,
    got: i32,
) -> Result<i32, Error> {
    if fd != 0 {
        return Ok(BAD_DESCRIPTOR);
    }
    let count = count as u32;
    if count > BUFFERS {
        return Ok(INVALID);
    }
    charge(&mut caller, CALL + BUFFER * u64::from(count))?;
    let mut total: u32 = 0;
    for i in 0..count {
        let Some((at, len)) = buffer(&caller, list as u32, i) else {
            return Ok(FAULT);
        };
        let take = (len as usize).min(caller.data().unread());
        charge(&mut caller, take as u64)?;
        let mut bytes = vec![0; take];
        let filled = caller.data_mut().read(&mut bytes);
        if write(&mut caller, at, &bytes[..filled]) != OK {
            return Ok(FAULT);
        }
        total = total.wrapping_add(filled as u32);
        if filled < len as usize {
            break;
        }
    }
    Ok(write(&mut caller, got as u32, &total.to_le_bytes()))
}

fn random_get<T>(mut caller: Caller<'_, T>, at: i32, len: i32) -> Result<i32, Error> {
    const CHUNK: [u8; 4096] = [0x5a; 4096];
    let (at, len) = (at as u32, len as u32);
    charge(&mut caller, CALL + u64::from(len))?;
    let mut done: u32 = 0;
    while done < len {
        let step = (len - done).min(CHUNK.len() as u32);
        if write(&mut caller, at.wrapping_add(done), &CHUNK[..step as usize]) != OK {
            return Ok(FAULT);
        }
        done += step;
    }
    Ok(OK)
}

/// A call that does nothing but answer `answer`.
fn fixed<T>(caller: &mut Caller<'_, T>, answer: i32) -> Result<i32, Error> {
    charge(caller, CALL)?;
    Ok(answer)
}

pub fn link<T: Guest + 'static>(linker: &mut Linker<T>) -> Result<(), LinkerError> {
    linker
        .func_wrap(
            MODULE,
            "clock_time_get",
            |mut caller: Caller<'_, T>, _clock: i32, _precision: i64, time: i32| {
                charge(&mut caller, CALL)?;
                let now = caller.data_mut().tick();
                Ok::<i32, Error>(write(&mut caller, time as u32, &now.to_le_bytes()))
            },
        )?
        .func_wrap(
            MODULE,
            "environ_get",
            |mut caller: Caller<'_, T>, _: i32, _: i32| fixed(&mut caller, OK),
        )?
        .func_wrap(MODULE, "environ_sizes_get", none::<T>)?
        .func_wrap(MODULE, "fd_close", |mut caller: Caller<'_, T>, _: i32| {
            fixed(&mut caller, BAD_DESCRIPTOR)
        })?
        .func_wrap(
            MODULE,
            "fd_fdstat_get",
            |mut caller: Caller<'_, T>, _: i32, _: i32| fixed(&mut caller, BAD_DESCRIPTOR),
        )?
        .func_wrap(MODULE, "fd_read", fd_read::<T>)?
        .func_wrap(
            MODULE,
            "fd_seek",
            |mut caller: Caller<'_, T>, _: i32, _: i64, _: i32, _: i32| {
                fixed(&mut caller, NOT_SEEKABLE)
            },
        )?
        .func_wrap(MODULE, "fd_write", fd_write::<T>)?
        .func_wrap(MODULE, "proc_exit", |code: i32| -> Result<(), Error> {
            Err(Error::i32_exit(code))
        })?
        .func_wrap(MODULE, "random_get", random_get::<T>)?;
    Ok(())
}
