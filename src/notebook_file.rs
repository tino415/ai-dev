use crate::model::Block;

pub struct SavedBlock {
    pub id: String,
    pub session: String,
    pub author: String,
    pub code: String,
}

pub fn serialize(notebook: &str, blocks: &[Block]) -> String {
    let mut text = format!("# {notebook}\n");
    for block in blocks.iter().filter(|block| block.state != crate::model::BlockState::Rejected) {
        let fence = "`".repeat(longest_backtick_run(&block.code).max(2) + 1);
        text.push_str(&format!(
            "\n{fence}sh session={} author={} id={}\n{}\n{fence}\n",
            block.session, block.author, block.id, block.code
        ));
    }
    text
}

pub fn parse(text: &str) -> Vec<SavedBlock> {
    let mut blocks = Vec::new();
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        let fence_len = line.chars().take_while(|c| *c == '`').count();
        let info = &line[fence_len..];
        if fence_len < 3 || !info.starts_with("sh ") {
            continue;
        }
        let fence = "`".repeat(fence_len);
        let code: Vec<&str> = lines
            .by_ref()
            .take_while(|inner| inner.trim_end() != fence)
            .collect();
        blocks.push(SavedBlock {
            id: field(info, "id"),
            session: field(info, "session"),
            author: field(info, "author"),
            code: code.join("\n"),
        });
    }
    blocks
}

fn field(info: &str, name: &str) -> String {
    let prefix = format!("{name}=");
    info.split_whitespace()
        .find_map(|part| part.strip_prefix(&prefix))
        .unwrap_or_default()
        .to_string()
}

fn longest_backtick_run(text: &str) -> usize {
    text.split(|c| c != '`').map(str::len).max().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_blocks_including_fences_in_code() {
        let mut block = Block::new("nb", "prod", "echo '```'\nls", "claude-1");
        block.id = "abc".into();
        let parsed = parse(&serialize("nb", &[block]));
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].code, "echo '```'\nls");
        assert_eq!((parsed[0].session.as_str(), parsed[0].author.as_str(), parsed[0].id.as_str()), ("prod", "claude-1", "abc"));
    }

    #[test]
    fn holds_no_output() {
        let mut block = Block::new("nb", "prod", "ls", "martin");
        block.output = Some("SECRET OUTPUT".into());
        assert!(!serialize("nb", &[block]).contains("SECRET OUTPUT"));
    }
}
