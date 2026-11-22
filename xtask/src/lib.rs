pub mod archive;
pub mod bench;
pub mod build;
pub mod generate;
pub mod github;
pub mod publish;

pub(crate) fn next_flag_value<'a>(
    args: &'a [String],
    index: &mut usize,
    flag: &str,
) -> Result<&'a str, String> {
    if *index + 1 >= args.len() {
        return Err(format!("{flag} requires a value"));
    }
    let val = args[*index + 1].as_str();
    *index += 2;
    Ok(val)
}
