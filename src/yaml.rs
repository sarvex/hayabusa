extern crate serde_derive;
extern crate yaml_rust;

use crate::detections::configs::{self, StoredStatic};
use crate::detections::message::AlertMessage;
use crate::detections::message::ERROR_LOG_STACK;
use crate::detections::utils;
use crate::filter::RuleExclude;
use compact_str::CompactString;
use hashbrown::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use yaml_rust::YamlLoader;

pub struct ParseYaml {
    pub files: Vec<(String, yaml_rust::Yaml)>,
    pub rulecounter: HashMap<CompactString, u128>,
    pub rule_load_cnt: HashMap<CompactString, u128>,
    pub rule_status_cnt: HashMap<CompactString, u128>,
    pub errorrule_count: u128,
    pub exclude_status: HashSet<String>,
    pub level_map: HashMap<String, u128>,
}

impl ParseYaml {
    pub fn new(stored_static: &StoredStatic) -> ParseYaml {
        let exclude_status_vec = if let Some(output_option) = stored_static.output_option.as_ref() {
            &output_option.exclude_status
        } else {
            &None
        };
        ParseYaml {
            files: Vec::new(),
            rulecounter: HashMap::new(),
            rule_load_cnt: HashMap::from([("excluded".into(), 0_u128), ("noisy".into(), 0_u128)]),
            rule_status_cnt: HashMap::from([
                ("deprecated".into(), 0_u128),
                ("unsupported".into(), 0_u128),
            ]),
            errorrule_count: 0,
            exclude_status: configs::convert_option_vecs_to_hs(exclude_status_vec.as_ref()),
            level_map: HashMap::from([
                ("INFORMATIONAL".to_owned(), 1),
                ("LOW".to_owned(), 2),
                ("MEDIUM".to_owned(), 3),
                ("HIGH".to_owned(), 4),
                ("CRITICAL".to_owned(), 5),
            ]),
        }
    }

    pub fn read_file(&self, path: PathBuf) -> Result<String, String> {
        let mut file_content = String::new();

        let mut fr = fs::File::open(path)
            .map(BufReader::new)
            .map_err(|e| e.to_string())?;

        fr.read_to_string(&mut file_content)
            .map_err(|e| e.to_string())?;

        Ok(file_content)
    }

    pub fn read_dir<P: AsRef<Path>>(
        &mut self,
        path: P,
        min_level: &str,
        target_level: &str,
        exclude_ids: &RuleExclude,
        stored_static: &StoredStatic,
    ) -> io::Result<String> {
        let metadata = fs::metadata(path.as_ref());
        if metadata.is_err() {
            let errmsg = format!(
                "fail to read metadata of file: {}",
                path.as_ref().to_path_buf().display(),
            );
            if stored_static.verbose_flag {
                AlertMessage::alert(&errmsg)?;
            }
            if !stored_static.quiet_errors_flag {
                ERROR_LOG_STACK
                    .lock()
                    .unwrap()
                    .push(format!("[ERROR] {errmsg}"));
            }
            return io::Result::Ok(String::default());
        }
        let mut yaml_docs = vec![];
        if metadata.unwrap().file_type().is_file() {
            // 拡張子がymlでないファイルは無視
            if path
                .as_ref()
                .to_path_buf()
                .extension()
                .unwrap_or_else(|| OsStr::new(""))
                != "yml"
            {
                return io::Result::Ok(String::default());
            }

            // 個別のファイルの読み込みは即終了としない。
            let read_content = self.read_file(path.as_ref().to_path_buf());
            if read_content.is_err() {
                let errmsg = format!(
                    "fail to read file: {}\n{} ",
                    path.as_ref().to_path_buf().display(),
                    read_content.unwrap_err()
                );
                if stored_static.verbose_flag {
                    AlertMessage::warn(&errmsg)?;
                }
                if !stored_static.quiet_errors_flag {
                    ERROR_LOG_STACK
                        .lock()
                        .unwrap()
                        .push(format!("[WARN] {errmsg}"));
                }
                self.errorrule_count += 1;
                return io::Result::Ok(String::default());
            }

            // ここも個別のファイルの読み込みは即終了としない。
            let yaml_contents = YamlLoader::load_from_str(&read_content.unwrap());
            if yaml_contents.is_err() {
                let errmsg = format!(
                    "Failed to parse yml: {}\n{} ",
                    path.as_ref().to_path_buf().display(),
                    yaml_contents.unwrap_err()
                );
                if stored_static.verbose_flag {
                    AlertMessage::warn(&errmsg)?;
                }
                if !stored_static.quiet_errors_flag {
                    ERROR_LOG_STACK
                        .lock()
                        .unwrap()
                        .push(format!("[WARN] {errmsg}"));
                }
                self.errorrule_count += 1;
                return io::Result::Ok(String::default());
            }

            yaml_docs.extend(yaml_contents.unwrap().into_iter().map(|yaml_content| {
                let filepath = format!("{}", path.as_ref().to_path_buf().display());
                (filepath, yaml_content)
            }));
        } else {
            let mut entries = fs::read_dir(path)?;
            yaml_docs = entries.try_fold(vec![], |mut ret, entry| {
                let entry = entry?;
                // フォルダは再帰的に呼び出す。
                if entry.file_type()?.is_dir() {
                    self.read_dir(
                        entry.path(),
                        min_level,
                        target_level,
                        exclude_ids,
                        stored_static,
                    )?;
                    return io::Result::Ok(ret);
                }
                // ファイル以外は無視
                if !entry.file_type()?.is_file() {
                    return io::Result::Ok(ret);
                }

                // 拡張子がymlでないファイルは無視
                let path = entry.path();
                if path.extension().unwrap_or_else(|| OsStr::new("")) != "yml" {
                    return io::Result::Ok(ret);
                }

                let path_str = path.to_str().unwrap();
                // ignore if yml file in .git folder.
                if utils::contains_str(path_str, "/.git/")
                    || utils::contains_str(path_str, "\\.git\\")
                {
                    return io::Result::Ok(ret);
                }

                // ignore if tool test yml file in hayabusa-rules.
                if utils::contains_str(path_str, "rules/tools/sigmac/test_files")
                    || utils::contains_str(path_str, "rules\\tools\\sigmac\\test_files")
                {
                    return io::Result::Ok(ret);
                }

                // 個別のファイルの読み込みは即終了としない。
                let read_content = self.read_file(path);
                if read_content.is_err() {
                    let errmsg = format!(
                        "fail to read file: {}\n{} ",
                        entry.path().display(),
                        read_content.unwrap_err()
                    );
                    if stored_static.verbose_flag {
                        AlertMessage::warn(&errmsg)?;
                    }
                    if !stored_static.quiet_errors_flag {
                        ERROR_LOG_STACK
                            .lock()
                            .unwrap()
                            .push(format!("[WARN] {errmsg}"));
                    }
                    self.errorrule_count += 1;
                    return io::Result::Ok(ret);
                }

                // ここも個別のファイルの読み込みは即終了としない。
                let yaml_contents = YamlLoader::load_from_str(&read_content.unwrap());
                if yaml_contents.is_err() {
                    let errmsg = format!(
                        "Failed to parse yml: {}\n{} ",
                        entry.path().display(),
                        yaml_contents.unwrap_err()
                    );
                    if stored_static.verbose_flag {
                        AlertMessage::warn(&errmsg)?;
                    }
                    if !stored_static.quiet_errors_flag {
                        ERROR_LOG_STACK
                            .lock()
                            .unwrap()
                            .push(format!("[WARN] {errmsg}"));
                    }
                    self.errorrule_count += 1;
                    return io::Result::Ok(ret);
                }

                let yaml_contents = yaml_contents.unwrap().into_iter().map(|yaml_content| {
                    let filepath = format!("{}", entry.path().display());
                    (filepath, yaml_content)
                });
                ret.extend(yaml_contents);
                io::Result::Ok(ret)
            })?;
        }

        let files = yaml_docs.into_iter().filter_map(|(filepath, yaml_doc)| {
            //除外されたルールは無視する
            let rule_id = &yaml_doc["id"].as_str();
            if rule_id.is_some() {
                if let Some(v) = exclude_ids
                    .no_use_rule
                    .get(&rule_id.unwrap_or(&String::default()).to_string())
                {
                    let entry_key = if utils::contains_str(v, "exclude_rule") {
                        "excluded"
                    } else {
                        "noisy"
                    };
                    // テスト用のルール(ID:000...0)の場合はexcluded ruleのカウントから除外するようにする
                    if v != "00000000-0000-0000-0000-000000000000" {
                        let entry = self.rule_load_cnt.entry(entry_key.into()).or_insert(0);
                        *entry += 1;
                    }
                    let enable_noisy_rules = if let Some(o) = stored_static.output_option.as_ref() {
                        o.enable_noisy_rules
                    } else {
                        false
                    };

                    if entry_key == "excluded" || (entry_key == "noisy" && !enable_noisy_rules) {
                        return Option::None;
                    }
                }
            }

            let mut up_rule_status_cnt = |status: &str| {
                let status_cnt = self.rule_status_cnt.entry(status.into()).or_insert(0);
                *status_cnt += 1;
            };

            let status = yaml_doc["status"].as_str();
            if let Some(s) = yaml_doc["status"].as_str() {
                // excluded status optionで指定されたstatusを除外する
                if self.exclude_status.contains(&s.to_string()) {
                    let entry = self.rule_load_cnt.entry("excluded".into()).or_insert(0);
                    *entry += 1;
                    return Option::None;
                }
                if stored_static.output_option.is_some()
                    && ((s == "deprecated"
                        && !stored_static
                            .output_option
                            .as_ref()
                            .unwrap()
                            .enable_deprecated_rules)
                        || (s == "unsupported"
                            && !stored_static
                                .output_option
                                .as_ref()
                                .unwrap()
                                .enable_unsupported_rules))
                {
                    // deprecated or unsupported statusで対応するenable-xxx-rules optionが指定されていない場合はステータスのカウントのみ行ったうえで除外する
                    up_rule_status_cnt(s);
                    return Option::None;
                }
            }

            self.rulecounter.insert(
                yaml_doc["ruletype"].as_str().unwrap_or("Other").into(),
                self.rulecounter
                    .get(yaml_doc["ruletype"].as_str().unwrap_or("Other"))
                    .unwrap_or(&0)
                    + 1,
            );

            up_rule_status_cnt(status.unwrap_or("undefined"));

            if stored_static.verbose_flag {
                println!("Loaded yml file path: {filepath}");
            }

            // 指定されたレベルより低いルールは無視する
            let doc_level = &yaml_doc["level"]
                .as_str()
                .unwrap_or("informational")
                .to_uppercase();
            let doc_level_num = self.level_map.get(doc_level).unwrap_or(&1);
            let args_level_num = self.level_map.get(min_level).unwrap_or(&1);
            let target_level_num = self.level_map.get(target_level).unwrap_or(&0);
            if doc_level_num < args_level_num
                || (target_level_num != &0_u128 && doc_level_num != target_level_num)
            {
                return Option::None;
            }

            Option::Some((filepath, yaml_doc))
        });
        self.files.extend(files);
        io::Result::Ok(String::default())
    }
}

#[cfg(test)]
mod tests {

    use crate::detections::configs::Action;
    use crate::detections::configs::CommonOptions;
    use crate::detections::configs::Config;
    use crate::detections::configs::CsvOutputOption;
    use crate::detections::configs::DetectCommonOption;
    use crate::detections::configs::InputOption;
    use crate::detections::configs::OutputOption;
    use crate::detections::configs::StoredStatic;
    use crate::filter;
    use crate::yaml;
    use crate::yaml::RuleExclude;
    use hashbrown::HashMap;
    use std::path::Path;
    use yaml_rust::YamlLoader;

    fn create_dummy_stored_static() -> StoredStatic {
        StoredStatic::create_static_data(Some(Config {
            action: Some(Action::CsvTimeline(CsvOutputOption {
                output_options: OutputOption {
                    input_args: InputOption {
                        directory: None,
                        filepath: None,
                        live_analysis: false,
                    },
                    profile: None,
                    enable_deprecated_rules: false,
                    exclude_status: None,
                    min_level: "informational".to_string(),
                    exact_level: None,
                    enable_noisy_rules: false,
                    end_timeline: None,
                    start_timeline: None,
                    eid_filter: false,
                    european_time: false,
                    iso_8601: false,
                    rfc_2822: false,
                    rfc_3339: false,
                    us_military_time: false,
                    us_time: false,
                    utc: false,
                    visualize_timeline: false,
                    rules: Path::new("./rules").to_path_buf(),
                    html_report: None,
                    no_summary: false,
                    common_options: CommonOptions {
                        no_color: false,
                        quiet: false,
                    },
                    detect_common_options: DetectCommonOption {
                        evtx_file_ext: None,
                        thread_number: None,
                        quiet_errors: false,
                        config: Path::new("./rules/config").to_path_buf(),
                        verbose: false,
                        json_input: false,
                    },
                    enable_unsupported_rules: false,
                },
                geo_ip: None,
                output: None,
                multiline: false,
            })),
            debug: false,
        }))
    }

    #[test]
    fn test_read_file_yaml() {
        let exclude_ids = RuleExclude::new();
        let dummy_stored_static = create_dummy_stored_static();
        let mut yaml = yaml::ParseYaml::new(&dummy_stored_static);
        let _ = &yaml.read_dir(
            "test_files/rules/yaml/1.yml",
            &String::default(),
            "",
            &exclude_ids,
            &dummy_stored_static,
        );
        assert_eq!(yaml.files.len(), 1);
    }

    #[test]
    fn test_read_dir_yaml() {
        let exclude_ids = RuleExclude {
            no_use_rule: HashMap::new(),
        };
        let dummy_stored_static = create_dummy_stored_static();
        let mut yaml = yaml::ParseYaml::new(&dummy_stored_static);
        let _ = &yaml.read_dir(
            "test_files/rules/yaml/",
            &String::default(),
            "",
            &exclude_ids,
            &dummy_stored_static,
        );
        assert_ne!(yaml.files.len(), 0);
    }

    #[test]
    fn test_read_yaml() {
        let yaml = yaml::ParseYaml::new(&create_dummy_stored_static());
        let path = Path::new("test_files/rules/yaml/1.yml");
        let ret = yaml.read_file(path.to_path_buf()).unwrap();
        let rule = YamlLoader::load_from_str(&ret).unwrap();
        for i in rule {
            if i["title"].as_str().unwrap() == "Sysmon Check command lines" {
                assert_eq!(
                    "*",
                    i["detection"]["selection"]["CommandLine"].as_str().unwrap()
                );
                assert_eq!(1, i["detection"]["selection"]["EventID"].as_i64().unwrap());
            }
        }
    }

    #[test]
    fn test_failed_read_yaml() {
        let yaml = yaml::ParseYaml::new(&create_dummy_stored_static());
        let path = Path::new("test_files/rules/yaml/error.yml");
        let ret = yaml.read_file(path.to_path_buf()).unwrap();
        let rule = YamlLoader::load_from_str(&ret);
        assert!(rule.is_err());
    }

    #[test]
    /// no specifed "level" arguments value is adapted default level(informational)
    fn test_default_level_read_yaml() {
        let path = Path::new("test_files/rules/level_yaml");
        let dummy_stored_static = create_dummy_stored_static();
        let mut yaml = yaml::ParseYaml::new(&dummy_stored_static);
        yaml.read_dir(
            path,
            "",
            "",
            &filter::exclude_ids(&dummy_stored_static),
            &dummy_stored_static,
        )
        .unwrap();
        assert_eq!(yaml.files.len(), 5);
    }

    #[test]
    fn test_info_level_read_yaml() {
        let dummy_stored_static = create_dummy_stored_static();
        let path = Path::new("test_files/rules/level_yaml");
        let mut yaml = yaml::ParseYaml::new(&dummy_stored_static);
        yaml.read_dir(
            path,
            "INFORMATIONAL",
            "",
            &filter::exclude_ids(&dummy_stored_static),
            &dummy_stored_static,
        )
        .unwrap();
        assert_eq!(yaml.files.len(), 5);
    }
    #[test]
    fn test_low_level_read_yaml() {
        let path = Path::new("test_files/rules/level_yaml");
        let dummy_stored_static = create_dummy_stored_static();
        let mut yaml = yaml::ParseYaml::new(&dummy_stored_static);
        yaml.read_dir(
            path,
            "LOW",
            "",
            &filter::exclude_ids(&dummy_stored_static),
            &dummy_stored_static,
        )
        .unwrap();
        assert_eq!(yaml.files.len(), 4);
    }
    #[test]
    fn test_medium_level_read_yaml() {
        let path = Path::new("test_files/rules/level_yaml");
        let dummy_stored_static = create_dummy_stored_static();
        let mut yaml = yaml::ParseYaml::new(&dummy_stored_static);
        yaml.read_dir(
            path,
            "MEDIUM",
            "",
            &filter::exclude_ids(&dummy_stored_static),
            &dummy_stored_static,
        )
        .unwrap();
        assert_eq!(yaml.files.len(), 3);
    }
    #[test]
    fn test_high_level_read_yaml() {
        let path = Path::new("test_files/rules/level_yaml");
        let dummy_stored_static = create_dummy_stored_static();
        let mut yaml = yaml::ParseYaml::new(&dummy_stored_static);
        yaml.read_dir(
            path,
            "HIGH",
            "",
            &filter::exclude_ids(&dummy_stored_static),
            &dummy_stored_static,
        )
        .unwrap();
        assert_eq!(yaml.files.len(), 2);
    }
    #[test]
    fn test_critical_level_read_yaml() {
        let path = Path::new("test_files/rules/level_yaml");
        let dummy_stored_static = create_dummy_stored_static();
        let mut yaml = yaml::ParseYaml::new(&dummy_stored_static);
        yaml.read_dir(
            path,
            "CRITICAL",
            "",
            &filter::exclude_ids(&dummy_stored_static),
            &dummy_stored_static,
        )
        .unwrap();
        assert_eq!(yaml.files.len(), 1);
    }
    #[test]
    fn test_all_exclude_rules_file() {
        let path = Path::new("test_files/rules/yaml");
        let dummy_stored_static = create_dummy_stored_static();
        let mut yaml = yaml::ParseYaml::new(&dummy_stored_static);
        yaml.read_dir(
            path,
            "",
            "",
            &filter::exclude_ids(&dummy_stored_static),
            &dummy_stored_static,
        )
        .unwrap();
        assert_eq!(yaml.rule_load_cnt.get("excluded").unwrap().to_owned(), 5);
    }
    #[test]
    fn test_all_noisy_rules_file() {
        let path = Path::new("test_files/rules/yaml");
        let dummy_stored_static = create_dummy_stored_static();
        let mut yaml = yaml::ParseYaml::new(&dummy_stored_static);
        yaml.read_dir(
            path,
            "",
            "",
            &filter::exclude_ids(&dummy_stored_static),
            &dummy_stored_static,
        )
        .unwrap();
        assert_eq!(yaml.rule_load_cnt.get("noisy").unwrap().to_owned(), 5);
    }
    #[test]
    fn test_none_exclude_rules_file() {
        let path = Path::new("test_files/rules/yaml");
        let dummy_stored_static = create_dummy_stored_static();
        let mut yaml = yaml::ParseYaml::new(&dummy_stored_static);
        let exclude_ids = RuleExclude::new();
        yaml.read_dir(path, "", "", &exclude_ids, &dummy_stored_static)
            .unwrap();
        assert_eq!(yaml.rule_load_cnt.get("excluded").unwrap().to_owned(), 0);
    }
    #[test]
    fn test_exclude_deprecated_rules_file() {
        let path = Path::new("test_files/rules/deprecated");
        let dummy_stored_static = create_dummy_stored_static();
        let mut yaml = yaml::ParseYaml::new(&dummy_stored_static);
        let exclude_ids = RuleExclude::new();
        yaml.read_dir(path, "", "", &exclude_ids, &dummy_stored_static)
            .unwrap();
        assert_eq!(
            yaml.rule_status_cnt.get("deprecated").unwrap().to_owned(),
            1
        );
    }

    #[test]
    fn test_exclude_unsupported_rules_file() {
        let path = Path::new("test_files/rules/unsupported");
        let dummy_stored_static = create_dummy_stored_static();
        let mut yaml = yaml::ParseYaml::new(&dummy_stored_static);
        let exclude_ids = RuleExclude::new();
        yaml.read_dir(path, "", "", &exclude_ids, &dummy_stored_static)
            .unwrap();
        assert_eq!(
            yaml.rule_status_cnt.get("unsupported").unwrap().to_owned(),
            1
        );
    }

    #[test]
    fn test_info_exact_level_read_yaml() {
        let dummy_stored_static = create_dummy_stored_static();
        let path = Path::new("test_files/rules/level_yaml");
        let mut yaml = yaml::ParseYaml::new(&dummy_stored_static);
        yaml.read_dir(
            path,
            "",
            "INFORMATIONAL",
            &filter::exclude_ids(&dummy_stored_static),
            &dummy_stored_static,
        )
        .unwrap();
        assert_eq!(yaml.files.len(), 1);
    }

    #[test]
    fn test_low_exact_level_read_yaml() {
        let path = Path::new("test_files/rules/level_yaml");
        let dummy_stored_static = create_dummy_stored_static();
        let mut yaml = yaml::ParseYaml::new(&dummy_stored_static);
        yaml.read_dir(
            path,
            "",
            "LOW",
            &filter::exclude_ids(&dummy_stored_static),
            &dummy_stored_static,
        )
        .unwrap();
        assert_eq!(yaml.files.len(), 1);
    }

    #[test]
    fn test_medium_exact_level_read_yaml() {
        let path = Path::new("test_files/rules/level_yaml");
        let dummy_stored_static = create_dummy_stored_static();
        let mut yaml = yaml::ParseYaml::new(&dummy_stored_static);
        yaml.read_dir(
            path,
            "",
            "MEDIUM",
            &filter::exclude_ids(&dummy_stored_static),
            &dummy_stored_static,
        )
        .unwrap();
        assert_eq!(yaml.files.len(), 1);
    }

    #[test]
    fn test_high_exact_level_read_yaml() {
        let path = Path::new("test_files/rules/level_yaml");
        let dummy_stored_static = create_dummy_stored_static();
        let mut yaml = yaml::ParseYaml::new(&dummy_stored_static);
        yaml.read_dir(
            path,
            "",
            "HIGH",
            &filter::exclude_ids(&dummy_stored_static),
            &dummy_stored_static,
        )
        .unwrap();
        assert_eq!(yaml.files.len(), 1);
    }

    #[test]
    fn test_critical_exact_level_read_yaml() {
        let path = Path::new("test_files/rules/level_yaml");
        let dummy_stored_static = create_dummy_stored_static();
        let mut yaml = yaml::ParseYaml::new(&dummy_stored_static);
        yaml.read_dir(
            path,
            "",
            "CRITICAL",
            &filter::exclude_ids(&dummy_stored_static),
            &dummy_stored_static,
        )
        .unwrap();
        assert_eq!(yaml.files.len(), 1);
    }
}
