//! Guest-side negative tests for the persisted revocation boundary (PRD L02).

use super::*;

fn write_store(bytes: &[u8]) -> Result<(), String> {
    write_file(link_abi::REVOKED_PATH, bytes)?;
    write_file(link_abi::REVOKED_COMMIT_PATH, &sha256::digest(bytes))
}

fn write_file(path: &str, bytes: &[u8]) -> Result<(), String> {
    let file = sys::fs_create(ROOT, path).map_err(|e| alloc::format!("create revocations: {e}"))?;
    let written = sys::fs_write(file, 0, bytes);
    sys::handle_close(file).ok();
    match written {
        Ok(n) if n == bytes.len() => Ok(()),
        other => Err(alloc::format!("write revocations: {other:?}")),
    }
}

fn attach(ch: Handle, access: u32) -> Result<LinkReply, String> {
    let root =
        sys::handle_dup(ROOT, access | rights::TRANSFER).map_err(|e| alloc::format!("dup root: {e}"))?;
    let mut hello = LinkRequest::new(lreq::HELLO);
    hello.abi_version = link_abi::ABI_VERSION;
    link_call_transferring(ch, &hello, Some(root))
}

fn with_service(check: impl FnOnce(Handle) -> Result<(), String>) -> Result<(), String> {
    let (ch, theirs) = sys::channel_create().map_err(|e| alloc::format!("channel: {e}"))?;
    let svc = sys::spawn(ROOT, "bin/spacelink", LINK_QUOTA, Some(theirs))
        .map_err(|e| alloc::format!("spawn: {e}"))?;
    let result = check(ch);
    let killed = sys::kill(svc);
    let waited = sys::wait(svc);
    sys::handle_close(svc).ok();
    sys::handle_close(ch).ok();
    let reset = write_store(b"");
    result?;
    killed.map_err(|e| alloc::format!("kill: {e}"))?;
    waited.map_err(|e| alloc::format!("wait: {e}"))?;
    reset
}

fn assert_locked(ch: Handle) -> Result<(), String> {
    for request in [
        LinkRequest::with_path(lreq::INDEX, CORPUS),
        LinkRequest::with_query(lreq::QUERY, "embargo"),
        LinkRequest::with_query(lreq::BUNDLE, "embargo"),
        LinkRequest::new(lreq::BUNDLE_ENTRY),
    ] {
        let reply = link_call(ch, &request)?;
        expect_status("retrieval with unavailable revocations", reply.status, Error::Denied)?;
        if !reply.text().is_empty() || !reply.path().is_empty() {
            return Err(String::from("a denied reply contains cached source data"));
        }
    }
    Ok(())
}

pub(super) fn run(r: &mut Runner, disk: bool) {
    r.run_if(
        disk,
        "no disk on this machine",
        "L02",
        "interrupted store replacement denies retrieval after restart",
        || {
            for commit_marker in [&[][..], &sha256::digest(b"/spaceos/docs/SECRET.TXT\n")[..]] {
                write_store(b"/spaceos/docs/SECRET.TXT\n")?;
                write_file(link_abi::REVOKED_COMMIT_PATH, commit_marker)?;
                write_file(link_abi::REVOKED_PATH, b"")?;
                with_service(|ch| {
                    let hello = attach(ch, rights::FS | rights::FS_WRITE)?;
                    expect_status("attach after interrupted replacement", hello.status, Error::DataLoss)?;
                    assert_locked(ch)?;
                    write_store(b"/spaceos/docs/SECRET.TXT\n")?;
                    attach(ch, rights::FS | rights::FS_WRITE)?
                        .result()
                        .map_err(|e| alloc::format!("repair: {e}"))?;
                    link_call(ch, &LinkRequest::with_path(lreq::INDEX, CORPUS))?
                        .result()
                        .map_err(|e| alloc::format!("index after repair: {e}"))?;
                    let hidden = link_call(ch, &LinkRequest::with_query(lreq::QUERY, "embargo"))?;
                    expect_status("revoked after store repair", hidden.status, Error::NotFound)
                })?;
            }
            Ok(())
        },
    );

    r.run_if(disk, "no disk on this machine", "L02", "malformed revocations fail closed on restart", || {
        let oversized = alloc::vec![b'x'; link_abi::MAX_DOCS * (link_abi::PATH_MAX + 1) + 1];
        for (bytes, error) in [
            (b"/spaceos/docs/SECRET.TXT\n\xff\n".as_slice(), Error::Invalid),
            (b"/spaceos/docs/SECRET.TXT".as_slice(), Error::Invalid),
            (b"relative/path\n".as_slice(), Error::Invalid),
            (oversized.as_slice(), Error::MsgSize),
        ] {
            write_store(bytes)?;
            with_service(|ch| {
                let hello = attach(ch, rights::FS | rights::FS_WRITE)?;
                expect_status("attach with malformed revocations", hello.status, error)?;
                assert_locked(ch)
            })?;
        }
        Ok(())
    });

    r.run_if(
        disk,
        "no disk on this machine",
        "L02",
        "failed reattach discards cached context and can recover",
        || {
            write_store(b"")?;
            with_service(|ch| {
                attach(ch, rights::FS | rights::FS_WRITE)?
                    .result()
                    .map_err(|e| alloc::format!("attach: {e}"))?;
                link_call(ch, &LinkRequest::with_path(lreq::INDEX, CORPUS))?
                    .result()
                    .map_err(|e| alloc::format!("index: {e}"))?;
                let mut bundle = LinkRequest::with_query(lreq::BUNDLE, "embargo");
                bundle.budget = 4096;
                if link_call(ch, &bundle)?.total == 0 {
                    return Err(String::from("test needs a cached secret chunk"));
                }
                // The replacement root cannot read the revocation store.
                let failed = attach(ch, rights::FS_WRITE)?;
                expect_status("attach without read permission", failed.status, Error::Denied)?;
                assert_locked(ch)?;

                write_store(b"/spaceos/docs/SECRET.TXT\n")?;
                attach(ch, rights::FS | rights::FS_WRITE)?
                    .result()
                    .map_err(|e| alloc::format!("reattach: {e}"))?;
                let stale = link_call(ch, &LinkRequest::new(lreq::BUNDLE_ENTRY))?;
                expect_status("bundle after reattach", stale.status, Error::NotFound)?;
                link_call(ch, &LinkRequest::with_path(lreq::INDEX, CORPUS))?
                    .result()
                    .map_err(|e| alloc::format!("reindex: {e}"))?;
                let hidden = link_call(ch, &LinkRequest::with_query(lreq::QUERY, "embargo"))?;
                expect_status("revoked after recovery", hidden.status, Error::NotFound)
            })
        },
    );

    r.run_if(
        disk,
        "no disk on this machine",
        "L02",
        "failed persistence never reports a durable revoke or forget",
        || {
            write_store(b"")?;
            with_service(|ch| {
                attach(ch, rights::FS)?.result().map_err(|e| alloc::format!("read-only attach: {e}"))?;
                link_call(ch, &LinkRequest::with_path(lreq::INDEX, CORPUS))?
                    .result()
                    .map_err(|e| alloc::format!("index: {e}"))?;
                let revoked = link_call(ch, &LinkRequest::with_path(lreq::REVOKE, SECRET))?;
                expect_status("revoke without write permission", revoked.status, Error::Denied)?;
                let forgotten = link_call(ch, &LinkRequest::new(lreq::FORGET))?;
                expect_status("forget without write permission", forgotten.status, Error::Denied)?;
                link_call(ch, &LinkRequest::with_path(lreq::INDEX, CORPUS))?
                    .result()
                    .map_err(|e| alloc::format!("reindex: {e}"))?;
                let hidden = link_call(ch, &LinkRequest::with_query(lreq::QUERY, "embargo"))?;
                expect_status("revoked after failed forget", hidden.status, Error::NotFound)?;
                let alias = link_call(ch, &LinkRequest::with_path(lreq::INDEX, "/spaceos/docs/."))?;
                expect_status("noncanonical index path", alias.status, Error::Invalid)?;
                let reattached = attach(ch, rights::FS | rights::FS_WRITE)?;
                expect_status(
                    "reattach cannot discard unsaved revocation",
                    reattached.status,
                    Error::Denied,
                )?;
                assert_locked(ch)
            })
        },
    );
}
