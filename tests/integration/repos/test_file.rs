#![allow(dead_code)]

use std::{fs, path::PathBuf};

/// A content-only test file. Checkpoints are always explicit in v2 tests so
/// hook ordering and session identity remain visible in the test itself.
pub struct TestFile<'a> {
    pub file_path: PathBuf,
    pub repo: &'a super::test_repo::TestRepo,
    expected_lines: Vec<String>,
}

impl<'a> TestFile<'a> {
    pub fn new_with_filename(
        file_path: PathBuf,
        expected_lines: Vec<String>,
        repo: &'a super::test_repo::TestRepo,
    ) -> Self {
        Self {
            file_path,
            repo,
            expected_lines,
        }
    }

    pub fn from_existing_file(file_path: PathBuf, repo: &'a super::test_repo::TestRepo) -> Self {
        let contents = fs::read_to_string(&file_path).expect("read test file");
        Self {
            file_path,
            repo,
            expected_lines: contents.lines().map(str::to_owned).collect(),
        }
    }

    pub fn set_contents<T: Into<String>>(&mut self, lines: Vec<T>) -> &mut Self {
        self.set_contents_no_stage(lines);
        self.stage();
        self
    }

    pub fn set_contents_no_stage<T: Into<String>>(&mut self, lines: Vec<T>) -> &mut Self {
        self.expected_lines = lines.into_iter().map(Into::into).collect();
        if let Some(parent) = self.file_path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&self.file_path, self.contents()).unwrap();
        self
    }

    pub fn contents(&self) -> String {
        self.expected_lines.join("\n")
    }

    pub fn stage(&self) {
        self.repo
            .git(&["add", self.file_path.to_str().unwrap()])
            .unwrap();
    }

    pub fn assert_contents_expected(&self) {
        assert_eq!(
            fs::read_to_string(&self.file_path).unwrap(),
            self.contents()
        );
    }

    pub fn assert_committed_contents(&self) {
        let relative = self.file_path.strip_prefix(self.repo.path()).unwrap();
        let object = format!("HEAD:{}", relative.to_string_lossy().replace('\\', "/"));
        let actual = self.repo.git(&["show", &object]).unwrap();
        assert_eq!(actual, self.contents());
    }

    pub fn assert_committed_lines<T: Into<String>>(&mut self, lines: Vec<T>) {
        self.expected_lines = lines.into_iter().map(Into::into).collect();
        let relative = self.file_path.strip_prefix(self.repo.path()).unwrap();
        let object = format!("HEAD:{}", relative.to_string_lossy().replace('\\', "/"));
        let actual = self.repo.git(&["show", &object]).unwrap();
        assert_eq!(actual.lines().collect::<Vec<_>>(), self.expected_lines);
    }

    pub fn insert_at<T: Into<String>>(&mut self, index: usize, lines: Vec<T>) -> &mut Self {
        self.expected_lines
            .splice(index..index, lines.into_iter().map(Into::into));
        self.set_contents(self.expected_lines.clone())
    }

    pub fn replace_at<T: Into<String>>(&mut self, index: usize, line: T) -> &mut Self {
        self.expected_lines[index] = line.into();
        self.set_contents(self.expected_lines.clone())
    }
}

#[macro_export]
macro_rules! lines {
    ($($line:expr),* $(,)?) => { vec![$(String::from($line)),*] };
}
