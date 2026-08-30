use reporigor_process_tree::CapturedStream;

pub(crate) fn render_stream(stream: &CapturedStream, limit: usize) -> String {
    let mut rendered = String::from_utf8_lossy(&stream.bytes).into_owned();
    if stream.truncated {
        rendered.insert_str(0, &format!("[output truncated to the last {limit} bytes]\n"));
    }
    rendered
}
