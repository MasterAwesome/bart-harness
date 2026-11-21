pub(crate) fn uuid_v7() -> std::io::Result<String> {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis());
    let millis = u64::try_from(millis).unwrap_or(u64::MAX);
    Ok(uuid::Builder::from_unix_timestamp_millis(millis, &bytes()?)
        .into_uuid()
        .to_string())
}

pub(crate) fn uuid_v4() -> std::io::Result<String> {
    Ok(uuid::Builder::from_random_bytes(bytes()?)
        .into_uuid()
        .to_string())
}

pub(crate) fn bytes<const N: usize>() -> std::io::Result<[u8; N]> {
    let mut bytes = [0; N];
    getrandom::fill(&mut bytes)?;
    Ok(bytes)
}
