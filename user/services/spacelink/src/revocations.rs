use alloc::string::String;
use alloc::vec::Vec;
use libspace::spaceabi::error::Error;
use libspace::spaceabi::link::{MAX_DOCS, PATH_MAX, REVOKED_COMMIT_PATH, REVOKED_PATH};
use libspace::{Handle, sha256, sys};

const STORE_MAX: usize = MAX_DOCS * (PATH_MAX + 1);

pub fn valid_path(path: &str) -> bool {
    path.starts_with('/')
        && path.len() <= PATH_MAX
        && path.is_ascii()
        && !path.bytes().any(|b| b.is_ascii_control() || b == b'\\')
        && path[1..].split('/').all(|part| !part.is_empty() && part != "." && part != "..")
}

fn read_file(root: Handle, path: &str, bytes: &mut [u8]) -> Result<Option<usize>, Error> {
    let file = match sys::fs_open(root, path) {
        Ok(file) => file,
        Err(Error::NotFound) => return Ok(None),
        Err(e) => return Err(e),
    };
    let read = (|| {
        let size = sys::fs_stat(file)?.size;
        if size > bytes.len() as u64 {
            return Err(Error::MsgSize);
        }
        let size = size as usize;
        let n = sys::fs_read(file, 0, &mut bytes[..size])?;
        if n != size {
            return Err(Error::DataLoss);
        }
        Ok(Some(n))
    })();
    sys::handle_close(file).ok();
    read
}

pub fn load(root: Handle) -> Result<Vec<String>, Error> {
    let mut committed = [0u8; 32];
    let marker = read_file(root, REVOKED_COMMIT_PATH, &mut committed)?;
    if marker.is_some_and(|n| n != committed.len()) {
        return Err(Error::DataLoss);
    }
    let mut bytes = [0u8; STORE_MAX];
    let Some(n) = read_file(root, REVOKED_PATH, &mut bytes)? else {
        return if marker.is_some() { Err(Error::DataLoss) } else { Ok(Vec::new()) };
    };
    if marker.is_some() && sha256::digest(&bytes[..n]) != committed {
        return Err(Error::DataLoss);
    }
    let text = core::str::from_utf8(&bytes[..n]).map_err(|_| Error::Invalid)?;
    if !text.is_empty() && !text.ends_with('\n') {
        return Err(Error::Invalid);
    }
    let mut revoked: Vec<String> = Vec::new();
    for path in text.split_terminator('\n') {
        if !valid_path(path) {
            return Err(Error::Invalid);
        }
        if revoked.iter().any(|p| p.eq_ignore_ascii_case(path)) {
            continue;
        }
        if revoked.len() == MAX_DOCS {
            return Err(Error::MsgSize);
        }
        revoked.try_reserve(1).map_err(|_| Error::NoMemory)?;
        let mut entry = String::new();
        entry.try_reserve(path.len()).map_err(|_| Error::NoMemory)?;
        entry.push_str(path);
        revoked.push(entry);
    }
    Ok(revoked)
}

fn write_file(root: Handle, path: &str, bytes: &[u8]) -> Result<(), Error> {
    let file = sys::fs_create(root, path)?;
    let written = sys::fs_write(file, 0, bytes);
    sys::handle_close(file).ok();
    if written? != bytes.len() {
        return Err(Error::DataLoss);
    }
    Ok(())
}

pub fn save(root: Handle, revoked: &[String]) -> Result<(), Error> {
    let mut text = String::new();
    text.try_reserve(revoked.iter().map(|p| p.len() + 1).sum()).map_err(|_| Error::NoMemory)?;
    for path in revoked {
        text.push_str(path);
        text.push('\n');
    }
    write_file(root, REVOKED_COMMIT_PATH, &[])?;
    write_file(root, REVOKED_PATH, text.as_bytes())?;
    write_file(root, REVOKED_COMMIT_PATH, &sha256::digest(text.as_bytes()))
}
