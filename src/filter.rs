#[derive(Debug)]
pub struct DocumentFilterOptions<'a> {
    pub filters: &'a [String],
    pub paths: &'a [String],
    pub has: &'a [String],
    pub missing: &'a [String],
}
