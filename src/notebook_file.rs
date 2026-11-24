use crate::model::Block;

const WHY: &str = "Why: ";

pub struct SavedBlock {
    pub id: String,
    pub session: String,
    pub author: String,
    pub why: Option<String>,
    pub code: String,
}

pub fn serialize(notebook: &str, blocks: &[Block]) -> String {
    let mut text = format!("# {notebook}\n");
    for block in blocks.iter().filter(|block| !block.state.is_dropped()) {
        text.push('\n');
        if let Some(why) = &block.why {
            text.push_str(&format!("{WHY}{}\n", one_line(why)));
        }
        let fence = "`".repeat(longest_backtick_run(&block.code).max(2) + 1);
        text.push_str(&format!(
            "{fence}sh session={} author={} id={}\n{}\n{fence}\n",
            block.session, block.author, block.id, block.code
        ));
    }
    text
}

#[derive(Debug, PartialEq, Eq)]
pub enum FileError {
    BlockWithoutId,
    RepeatedId(String),
}

impl FileError {
    pub fn message(&self, notebook: &str) -> String {
        match self {
            FileError::BlockWithoutId => format!("{notebook}.md: a block has no id"),
            FileError::RepeatedId(id) => format!("{notebook}.md: block id {id} is used twice"),
        }
    }
}

pub fn parse(text: &str) -> Result<Vec<SavedBlock>, FileError> {
    let blocks = parse_blocks(text);
    let mut seen = std::collections::HashSet::new();
    for block in &blocks {
        if block.id.is_empty() {
            return Err(FileError::BlockWithoutId);
        }
        if !seen.insert(block.id.as_str()) {
            return Err(FileError::RepeatedId(block.id.clone()));
        }
    }
    Ok(blocks)
}

fn parse_blocks(text: &str) -> Vec<SavedBlock> {
    let mut blocks = Vec::new();
    let mut lines = text.lines();
    let mut why = None;
    while let Some(line) = lines.next() {
        let fence_len = line.chars().take_while(|c| *c == '`').count();
        let info = &line[fence_len..];
        if fence_len < 3 || !info.starts_with("sh ") {
            why = line.strip_prefix(WHY).map(str::to_string);
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
            why: why.take(),
            code: code.join("\n"),
        });
    }
    blocks
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
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
        let parsed = parse(&serialize("nb", &[block])).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].code, "echo '```'\nls");
        assert_eq!((parsed[0].session.as_str(), parsed[0].author.as_str(), parsed[0].id.as_str()), ("prod", "claude-1", "abc"));
    }

    #[test]
    fn holds_no_output() {
        let mut block = Block::new("nb", "prod", "ls", "human");
        block.output = Some("SECRET OUTPUT".into());
        assert!(!serialize("nb", &[block]).contains("SECRET OUTPUT"));
    }

    #[test]
    fn keeps_the_why_on_one_line_above_its_block_only() {
        let mut explained = Block::new("nb", "prod", "ls", "claude-1");
        explained.why = Some("check the\nlogs".into());
        let plain = Block::new("nb", "prod", "pwd", "human");
        let parsed = parse(&serialize("nb", &[explained, plain])).unwrap();
        assert_eq!(parsed[0].why.as_deref(), Some("check the logs"));
        assert_eq!(parsed[1].why, None);
    }

    #[test]
    fn loads_a_file_written_before_blocks_had_a_why() {
        let parsed = parse("# nb\n\n```sh session=prod author=human id=abc\nls\n```\n").unwrap();
        assert_eq!((parsed[0].code.as_str(), parsed[0].why.as_deref()), ("ls", None));
    }

    #[test]
    fn a_block_without_an_id_is_an_error() {
        let error = parse("# nb\n\n```sh session=prod author=human\nls\n```\n").err().unwrap();
        assert_eq!(error.message("nb"), "nb.md: a block has no id");
    }

    #[test]
    fn an_id_used_twice_is_an_error() {
        let text = "# nb\n\n```sh session=prod author=human id=abc\nls\n```\n\n```sh session=prod author=human id=abc\npwd\n```\n";
        assert_eq!(parse(text).err().unwrap().message("nb"), "nb.md: block id abc is used twice");
    }
}
