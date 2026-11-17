//! The second batch of apply-phase directives: array/object/class edits, `Replace Function`
//! (and its signature), `Delete Block`/`Span`/`In Passage`, tags, `Rename Passage`,
//! `Add Variable` and `Hook`. Ports of the matching branches of `patch_html_file`.

use super::*;
use crate::search::FastFind;

impl<'a> P<'a> {
    /// Failure that is logged to ModPatchLog only (the Python code counts it as failed but does not
    /// write it to FailsPatchLog).
    fn soft_fail(&mut self, msg: String) {
        self.out.log.push(msg);
        self.out.replacements_failed += 1;
    }

    // ------------------------------------------------------------------
    // array / object / class directives (`mod_struct_list`)
    // ------------------------------------------------------------------
    pub(super) fn structs(&mut self) -> Result<()> {
        let st = self.st;
        for e in &st.mod_struct_list {
            if !self.guards_ok(&e.guards)? {
                self.out.log.push(format!("Guard condition not met for '{} [{}]', skipping.", e.kind, e.name));
                continue;
            }
            match e.kind.as_str() {
                "append_array" => self.append_array(e),
                "append_var" => self.append_var(e),
                "append_func" => self.append_func(e),
                "insert_into_array" => self.insert_into_container(e, '[', ']', "Insert Into Array"),
                "insert_into_object" => self.insert_into_container(e, '{', '}', "Insert Into Object"),
                "merge_into_object" => self.merge_into_object(e),
                k => bail!("{}: struct kind '{k}' is not supported by the builtin engine yet", e.mod_file),
            }
        }
        Ok(())
    }

    fn append_array(&mut self, e: &StructEntry) {
        let key = format!("{} [{}]", e.kind, e.name);
        let re = Regex::new(&format!(r"{}\s*[=:]\s*\[", regex::escape(&e.name))).unwrap();
        let matches: Vec<usize> = re.find_iter(&self.html).map(|m| m.start()).collect();
        if matches.is_empty() {
            self.fail(format!("No match found for '{key}'"), true);
            return;
        }
        if self.html.contains(&e.content) {
            self.out.log.push(format!("Patch already applied for '{key}', skipping."));
            return;
        }
        let mut applied = 0usize;
        for &ms in matches.iter().rev() {
            let Some(open) = self.html[ms..].find('[').map(|i| ms + i) else { continue };
            let Some(close) = bracket_match(&self.html, open, b'[', b']') else { continue };
            self.html.insert_str(close, &format!("\n{}\n", e.content));
            applied += 1;
        }
        if applied > 0 {
            self.out.log.push(format!("Appended to array '{}' ({applied} occurrence(s))", e.name));
            self.out.replacements_made += 1;
        } else {
            self.fail(format!("Could not find closing ] for '{key}'"), true);
        }
    }

    /// Locate `name = function(...) {` / `function name(...) {`'s opening brace without failing.
    fn function_brace(&self, name: &str) -> Option<usize> {
        let n = regex::escape(name);
        let re = Regex::new(&format!(r"(?s)({n}\s*[=:]\s*function\s*\(.*?\)\s*\{{|function\s+{n}\s*\(.*?\)\s*\{{)")).unwrap();
        re.find(&self.html).map(|m| m.end() - 1)
    }

    fn append_var(&mut self, e: &StructEntry) {
        let key = format!("{} [{}]", e.kind, e.name);
        let n = regex::escape(&e.name);
        let class_re = Regex::new(&format!(r"(?s)class\s+{n}\b[^{{]*\{{")).unwrap();
        let insert_pos;
        if let Some(cm) = class_re.find(&self.html) {
            let class_open = cm.start() + self.html[cm.start()..].find('{').unwrap();
            let Some(class_close) = bracket_match(&self.html, class_open, b'{', b'}') else {
                self.fail(format!("Could not find class body end for '{key}'"), true);
                return;
            };
            let class_body = self.html[class_open..=class_close].to_string();
            let ctor_re = Regex::new(r"\bconstructor\s*\(").unwrap();
            let Some(ctor_m) = ctor_re.find(&class_body) else {
                self.soft_fail(format!("No constructor found in class '{}' for '{key}'", e.name));
                return;
            };
            let ctor_open_rel = ctor_m.start() + class_body[ctor_m.start()..].find('(').unwrap();
            let Some(paren_close) = bracket_match(&class_body, ctor_open_rel, b'(', b')') else {
                self.soft_fail(format!("Could not match constructor params for '{key}'"));
                return;
            };
            let Some(brace_start_rel) = class_body[paren_close..].find('{').map(|i| paren_close + i) else {
                self.soft_fail(format!("Could not match constructor body for '{key}'"));
                return;
            };
            let Some(brace_end_rel) = bracket_match(&class_body, brace_start_rel, b'{', b'}') else {
                self.soft_fail(format!("Could not match constructor body for '{key}'"));
                return;
            };
            insert_pos = class_open + brace_end_rel;
        } else {
            let func_re = Regex::new(&format!(r"function\s+{n}\s*\(")).unwrap();
            let Some(fm) = func_re.find(&self.html) else {
                self.fail(format!("No match found for '{key}'"), true);
                return;
            };
            let paren_open = fm.start() + self.html[fm.start()..].find('(').unwrap();
            let Some(paren_close) = bracket_match(&self.html, paren_open, b'(', b')') else {
                self.soft_fail(format!("Could not match param list for '{key}'"));
                return;
            };
            let Some(brace_open) = self.html[paren_close..].find('{').map(|i| paren_close + i) else {
                self.soft_fail(format!("Could not find opening brace for '{key}'"));
                return;
            };
            let Some(brace_close) = bracket_match(&self.html, brace_open, b'{', b'}') else {
                self.soft_fail(format!("Could not match function body for '{key}'"));
                return;
            };
            insert_pos = brace_close;
        }
        if self.html.contains(&e.content) {
            self.out.log.push(format!("Patch already applied for '{key}', skipping."));
            return;
        }
        self.html.insert_str(insert_pos, &format!("\n    {}\n", e.content));
        self.out.log.push(format!("Appended variable to class/function '{}'", e.name));
        self.out.replacements_made += 1;
    }

    fn append_func(&mut self, e: &StructEntry) {
        let key = format!("{} [{}]", e.kind, e.name);
        let n = regex::escape(&e.name);
        let class_re = Regex::new(&format!(r"(?s)class\s+{n}\b[^{{]*\{{")).unwrap();
        if let Some(cm) = class_re.find(&self.html) {
            let class_open = cm.start() + self.html[cm.start()..].find('{').unwrap();
            let Some(class_close) = bracket_match(&self.html, class_open, b'{', b'}') else {
                self.fail(format!("Could not find class body end for '{key}'"), true);
                return;
            };
            if self.html.contains(&e.content) {
                self.out.log.push(format!("Patch already applied for '{key}', skipping."));
                return;
            }
            // indent every non-blank line by 4 spaces (Python: content.splitlines())
            let indented: Vec<String> = crate::pytext::splitlines_keepends(&e.content)
                .iter()
                .map(|l| {
                    let line = l.trim_end_matches(['\n', '\r']);
                    if py_strip(line).is_empty() { line.to_string() } else { format!("    {line}") }
                })
                .collect();
            self.html.insert_str(class_close, &format!("\n{}\n", indented.join("\n")));
            self.out.log.push(format!("Appended function to class '{}'", e.name));
            self.out.replacements_made += 1;
        } else {
            let func_re = Regex::new(&format!(r"function\s+{n}\s*\(")).unwrap();
            let Some(fm) = func_re.find(&self.html) else {
                self.fail(format!("No match found for '{key}'"), true);
                return;
            };
            let paren_open = fm.start() + self.html[fm.start()..].find('(').unwrap();
            let Some(paren_close) = bracket_match(&self.html, paren_open, b'(', b')') else {
                self.soft_fail(format!("Could not match param list for '{key}'"));
                return;
            };
            let Some(brace_open) = self.html[paren_close..].find('{').map(|i| paren_close + i) else {
                self.soft_fail(format!("Could not find opening brace for '{key}'"));
                return;
            };
            let Some(brace_close) = bracket_match(&self.html, brace_open, b'{', b'}') else {
                self.soft_fail(format!("Could not match function body for '{key}'"));
                return;
            };
            if self.html.contains(&e.content) {
                self.out.log.push(format!("Patch already applied for '{key}', skipping."));
                return;
            }
            self.html.insert_str(brace_close + 1, &format!("\n{}.prototype.{}\n", e.name, e.content.trim_end()));
            self.out.log.push(format!("Appended prototype function to '{}'", e.name));
            self.out.replacements_made += 1;
        }
    }

    /// Search space for the optional `In Function [f]` scope: `(offset, end)`; whole file when unscoped.
    fn struct_scope(&mut self, scope_func: &Option<String>, label: &str) -> (usize, usize) {
        if let Some(sf) = scope_func {
            match self.function_brace(sf) {
                Some(open) => match bracket_match(&self.html, open, b'{', b'}') {
                    Some(end) => return (open, end + 1),
                    None => self.out.log.push(format!("{label}: could not find body of scope function '{sf}'")),
                },
                None => self.out.log.push(format!("{label}: scope function '{sf}' not found, searching globally")),
            }
        }
        (0, self.html.len())
    }

    fn insert_into_container(&mut self, e: &StructEntry, open_ch: char, close_ch: char, label: &str) {
        let (offset, end) = self.struct_scope(&e.scope_func, label);
        let n = regex::escape(&e.name);
        let open_re = if open_ch == '[' { r"\[" } else { r"\{" };
        let re = Regex::new(&format!(r"{n}\s*[=:]\s*{open_re}")).unwrap();
        let space = self.html[offset..end].to_string();
        let matches: Vec<usize> = re.find_iter(&space).map(|m| m.start()).collect();
        if matches.is_empty() {
            self.fail(format!("{label}: no match for '{}'", e.name), true);
            return;
        }
        if space.contains(&e.content) {
            self.out.log.push(format!("{label}: already applied for '{}', skipping.", e.name));
            return;
        }
        let mut applied = 0usize;
        for &ms in matches.iter().rev() {
            let abs = offset + ms;
            let Some(open) = self.html[abs..].find(open_ch).map(|i| abs + i) else { continue };
            let Some(close) = bracket_match(&self.html, open, open_ch as u8, close_ch as u8) else { continue };
            self.html.insert_str(close, &format!("\n{}\n", e.content));
            applied += 1;
        }
        if applied > 0 {
            self.out.log.push(format!("{label}: inserted into '{}' ({applied} occurrence(s))", e.name));
            self.out.replacements_made += 1;
        } else {
            self.fail(format!("{label}: could not find closing {close_ch} for '{}'", e.name), true);
        }
    }

    fn merge_into_object(&mut self, e: &StructEntry) {
        let (offset, end) = self.struct_scope(&e.scope_func, "Merge Into Object");
        let n = regex::escape(&e.name);
        let re = Regex::new(&format!(r"{n}\s*[=:]\s*\{{")).unwrap();
        let space = self.html[offset..end].to_string();
        let matches: Vec<usize> = re.find_iter(&space).map(|m| m.start()).collect();
        if matches.is_empty() {
            self.fail(format!("Merge Into Object: no match for '{}'", e.name), true);
            return;
        }
        let key_re = Regex::new(r#"^(["']?[\w$]+["']?)\s*:"#).unwrap();
        let mut applied = 0usize;
        for &ms in matches.iter().rev() {
            let abs = offset + ms;
            let Some(open) = self.html[abs..].find('{').map(|i| abs + i) else { continue };
            let Some(close) = bracket_match(&self.html, open, b'{', b'}') else { continue };
            let obj_body = self.html[open + 1..close].to_string();
            let mut lines: Vec<String> = Vec::new();
            for kv in crate::pytext::splitlines_keepends(&e.content) {
                let kv = py_strip(kv).trim_end_matches(',');
                let kv = py_strip(kv);
                if kv.is_empty() {
                    continue;
                }
                if let Some(c) = key_re.captures(kv) {
                    let key_str = c[1].trim_matches(|ch| ch == '"' || ch == '\'');
                    let exist = Regex::new(&format!(r"\b{}\b\s*:", regex::escape(key_str))).unwrap();
                    if exist.is_match(&obj_body) {
                        self.out.log.push(format!("Merge Into Object: key '{key_str}' already exists in '{}', skipping.", e.name));
                        continue;
                    }
                }
                lines.push(format!("{kv},"));
            }
            if lines.is_empty() {
                self.out.log.push(format!("Merge Into Object: all keys already present in '{}', skipping.", e.name));
                continue;
            }
            self.html.insert_str(close, &format!("\n{}\n", lines.join("\n")));
            applied += 1;
        }
        if applied > 0 {
            self.out.log.push(format!("Merge Into Object: merged into '{}' ({applied} occurrence(s))", e.name));
            self.out.replacements_made += 1;
        }
    }

    // ------------------------------------------------------------------
    // function / passage directives
    // ------------------------------------------------------------------
    pub(super) fn replace_func(&mut self, e: &FuncEntry) -> Result<()> {
        if !self.guards_ok(&e.guards)? {
            self.out.log.push(format!("Guard condition not met for 'Replace Function [{}]', skipping.", e.name));
            return Ok(());
        }
        let n = regex::escape(&e.name);
        let re = Regex::new(&format!(r"(?s)({n}\s*[=:]\s*function\s*\(.*?\)\s*\{{|function\s+{n}\s*\(.*?\)\s*\{{)")).unwrap();
        let Some(m) = re.find(&self.html) else {
            self.fail(format!("Replace Function: no match for '{}'", e.name), true);
            return Ok(());
        };
        let brace_open = m.end() - 1;
        let Some(brace_close) = bracket_match(&self.html, brace_open, b'{', b'}') else {
            self.soft_fail(format!("Replace Function: could not match body for '{}'", e.name));
            return Ok(());
        };
        let new_body = e.body.clone().unwrap_or_default();
        if self.html[m.start()..=brace_close].contains(&new_body) {
            self.out.log.push(format!("Replace Function: already applied for '{}', skipping.", e.name));
            return Ok(());
        }
        let replacement = format!("\n{new_body}\n");
        self.html.replace_range(brace_open + 1..brace_close, &replacement);
        self.out.log.push(format!("Replace Function: replaced body of '{}'", e.name));
        self.out.replacements_made += 1;
        Ok(())
    }

    pub(super) fn replace_func_sig(&mut self, e: &FuncEntry) -> Result<()> {
        if !self.guards_ok(&e.guards)? {
            self.out.log.push(format!("Guard condition not met for 'Replace Function Signature [{}]', skipping.", e.name));
            return Ok(());
        }
        let n = regex::escape(&e.name);
        let re = Regex::new(&format!(r"(?s)({n}\s*[=:]\s*function\s*\()([^)]*)(\))|(function\s+{n}\s*\()([^)]*)(\))")).unwrap();
        let Some(c) = re.captures(&self.html) else {
            self.fail(format!("Replace Function Signature: '{}' not found", e.name), true);
            return Ok(());
        };
        let g = c.get(2).or_else(|| c.get(5)).unwrap();
        let (ps, pe) = (g.start(), g.end());
        let new_params = e.new_params.clone().unwrap_or_default();
        if py_strip(&self.html[ps..pe]) == py_strip(&new_params) {
            self.out.log.push(format!("Replace Function Signature: already applied for '{}', skipping.", e.name));
            return Ok(());
        }
        self.html.replace_range(ps..pe, &new_params);
        self.out.log.push(format!("Replace Function Signature: updated params of '{}' -> ({})", e.name, new_params));
        self.out.replacements_made += 1;
        Ok(())
    }

    pub(super) fn delete_block(&mut self, e: &FuncEntry) -> Result<()> {
        if !self.guards_ok(&e.guards)? {
            self.out.log.push(format!("Guard condition not met for 'Delete Block [{}]', skipping.", e.name));
            return Ok(());
        }
        let Some(sp) = find_passage(&self.html, &e.name) else {
            self.fail(format!("Delete Block: no passage named '{}' found", e.name), true);
            return Ok(());
        };
        self.html.replace_range(sp.open_start..sp.close_end, "");
        self.original_names.remove(&e.name);
        self.out.log.push(format!("Delete Block: deleted passage '{}'", e.name));
        self.out.replacements_made += 1;

        // early-inject the replacement passage from the registry (same name)
        if let Some(&idx) = self.names_seen.get(&e.name) {
            let rec = self.registry[idx].clone();
            if self.passage_guards_ok(&rec)? {
                let body = escape_html_between_tags(&rec.body);
                let body = escape_twine_tags(&body);
                let body = auto_escape_passage_bodies(&body);
                let tag = format!(
                    "<tw-passagedata pid=\"auto\" name=\"{}\" tags=\"{}\" position=\"100,100\" size=\"100,100\">{}</tw-passagedata>",
                    e.name, rec.tags, body
                );
                if let Some(close) = self.html.ffind("</tw-storydata>") {
                    self.html.insert_str(close, &format!("\n{tag}\n"));
                }
                self.early_injected.insert(idx);
                self.out.log.push(format!("Delete Block: early-injected replacement '{}' from {}", e.name, rec.mod_file));
            } else {
                self.out.log.push(format!("Delete Block: replacement '{}' in registry but guard not met, deferring to Phase 7", e.name));
            }
        }
        Ok(())
    }

    pub(super) fn delete_span(&mut self, e: &FuncEntry) -> Result<()> {
        let start_anchor = escape_twine_tags(&e.name);
        let end_anchor = escape_twine_tags(e.end_anchor.as_deref().unwrap_or(""));
        if !self.guards_ok(&e.guards)? {
            self.out.log.push(format!("Guard condition not met for 'Delete Span [{}]', skipping.", short(&start_anchor, 40)));
            return Ok(());
        }
        let Some(start_idx) = self.html.ffind(&start_anchor) else {
            self.fail(format!("Delete Span: start anchor not found: '{}'", short(&start_anchor, 60)), true);
            return Ok(());
        };
        let end_search = start_idx + start_anchor.len();
        let Some(end_rel) = self.html[end_search..].find(&end_anchor) else {
            self.fail(format!("Delete Span: end anchor not found after start: '{}'", short(&end_anchor, 60)), true);
            return Ok(());
        };
        let span_end = end_search + end_rel + end_anchor.len();
        let after_span = window(&self.html, span_end, 20).to_string();
        let after_start = window(&self.html, end_search, 20).to_string();
        if after_start == after_span {
            self.out.log.push("Delete Span: already applied, skipping.".to_string());
            return Ok(());
        }
        let deleted = span_end - start_idx;
        self.html.replace_range(start_idx..span_end, "");
        self.out.log.push(format!("Delete Span: deleted {deleted} bytes from '{}' to '{}'", short(&start_anchor, 40), short(&end_anchor, 40)));
        self.out.replacements_made += 1;
        Ok(())
    }

    pub(super) fn delete_in_passage(&mut self, e: &FuncEntry) -> Result<()> {
        let pname = e.passage_name.clone().unwrap_or_default();
        if !self.guards_ok(&e.guards)? {
            self.out.log.push(format!("Guard condition not met for 'Delete In Passage [{pname}]', skipping."));
            return Ok(());
        }
        check_literal_anchor(&e.name)?;
        let Some(sp) = find_passage(&self.html, &pname) else {
            self.fail(format!("Delete In Passage [{pname}]: passage not found"), true);
            return Ok(());
        };
        let body = &self.html[sp.body_start..sp.body_end];
        let Some(rel) = body.find(&e.name) else {
            self.fail(format!("Delete In Passage [{pname}]: anchor not found in passage body"), true);
            return Ok(());
        };
        let abs = sp.body_start + rel;
        self.html.replace_range(abs..abs + e.name.len(), "");
        self.out.log.push(format!("Delete In Passage [{pname}]: deleted '{}'", short(&e.name, 60)));
        self.out.replacements_made += 1;
        Ok(())
    }

    /// `(tag start, tag end exclusive, tags-attr value range)` for the first passage whose opening
    /// tag has `name="<name>"` followed by `tags="..."`.
    fn find_tags_attr(&self, name: &str) -> Option<(usize, usize, usize, usize)> {
        let needle = format!("name=\"{name}\"");
        let mut from = 0usize;
        while let Some(rel) = self.html[from..].find(&needle) {
            let p = from + rel;
            if let Some(ts) = self.html[..p].rfind(PASSAGE_OPEN) {
                if !self.html[ts..p].contains('>') {
                    let after = p + needle.len();
                    if let Some(g) = self.html[after..].find('>') {
                        let tag_rest = &self.html[after..after + g];
                        if let Some(tpos) = tag_rest.rfind("tags=\"") {
                            let vstart = after + tpos + 6;
                            if let Some(q) = self.html[vstart..after + g].find('"') {
                                return Some((after + tpos, vstart + q + 1, vstart, vstart + q));
                            }
                        }
                    }
                }
            }
            from = p + 1;
        }
        None
    }

    pub(super) fn tag_op(&mut self, e: &FuncEntry, add: bool) -> Result<()> {
        let pname = e.passage_name.clone().unwrap_or_default();
        let what = if add { "Add Tag To Passage" } else { "Remove Tag From Passage" };
        if !self.guards_ok(&e.guards)? {
            self.out.log.push(format!("Guard condition not met for '{what} [{pname}]', skipping."));
            return Ok(());
        }
        let wanted: Vec<String> = e.tags.clone().unwrap_or_default().split_whitespace().map(|s| s.to_string()).collect();
        let Some((attr_start, attr_end, v_start, v_end)) = self.find_tags_attr(&pname) else {
            self.fail(format!("{what}: passage '{pname}' not found"), true);
            return Ok(());
        };
        let current: Vec<String> = self.html[v_start..v_end].split_whitespace().map(|s| s.to_string()).collect();
        let new_attr;
        if add {
            let to_add: Vec<&String> = wanted.iter().filter(|t| !current.contains(t)).collect();
            if to_add.is_empty() {
                self.out.log.push(format!("{what} [{pname}]: all tags already present, skipping."));
                return Ok(());
            }
            let mut all = current.clone();
            all.extend(to_add.into_iter().cloned());
            new_attr = format!("tags=\"{}\"", all.join(" "));
        } else {
            let remaining: Vec<String> = current.iter().filter(|t| !wanted.contains(t)).cloned().collect();
            if remaining == current {
                self.out.log.push(format!("{what} [{pname}]: no matching tags found, skipping."));
                return Ok(());
            }
            new_attr = format!("tags=\"{}\"", remaining.join(" "));
        }
        self.html.replace_range(attr_start..attr_end, &new_attr);
        self.out.log.push(format!("{what}: updated tags of '{pname}'"));
        self.out.replacements_made += 1;
        Ok(())
    }

    pub(super) fn rename_passage(&mut self, e: &FuncEntry) -> Result<()> {
        if !self.guards_ok(&e.guards)? {
            self.out.log.push(format!("Guard condition not met for 'Rename Passage [{}]', skipping.", e.name));
            return Ok(());
        }
        let needle = format!("name=\"{}\"", e.name);
        let mut from = 0usize;
        let mut hit: Option<usize> = None;
        while let Some(rel) = self.html[from..].find(&needle) {
            let p = from + rel;
            if let Some(ts) = self.html[..p].rfind(PASSAGE_OPEN) {
                if !self.html[ts..p].contains('>') {
                    hit = Some(p);
                    break;
                }
            }
            from = p + 1;
        }
        let Some(p) = hit else {
            self.fail(format!("Rename Passage: no passage named '{}' found", e.name), true);
            return Ok(());
        };
        let new_name = e.new_name.clone().unwrap_or_default();
        let start = p + "name=\"".len();
        self.html.replace_range(start..start + e.name.len(), &new_name);
        self.out.log.push(format!("Rename Passage: renamed '{}' -> '{new_name}'", e.name));
        self.out.replacements_made += 1;
        Ok(())
    }

    pub(super) fn add_variable(&mut self, e: &FuncEntry) -> Result<()> {
        if !self.guards_ok(&e.guards)? {
            self.out.log.push(format!("Guard condition not met for 'Add Variable [{}]', skipping.", e.name));
            return Ok(());
        }
        let Some(sp) = find_passage(&self.html, "StoryInit") else {
            self.fail(format!("Add Variable: StoryInit passage not found for '{}'", e.name), true);
            return Ok(());
        };
        let var_re = Regex::new(&format!(r"\${}\b", regex::escape(&e.name))).unwrap();
        if var_re.is_match(&self.html[sp.body_start..sp.body_end]) {
            self.out.log.push(format!("Add Variable: '{}' already in StoryInit, skipping.", e.name));
            return Ok(());
        }
        let value = e.value.clone().unwrap_or_else(|| "undefined".to_string());
        let set_line = format!("&lt;&lt;set ${} to {}&gt;&gt;\n", e.name, value);
        self.html.insert_str(sp.body_start, &set_line);
        self.out.log.push(format!("Add Variable: injected '${}' into StoryInit", e.name));
        self.out.replacements_made += 1;
        Ok(())
    }

    // ------------------------------------------------------------------
    // Hook [fn]: before / after / around
    // ------------------------------------------------------------------
    pub(super) fn hooks(&mut self) -> Result<()> {
        let st = self.st;
        for h in &st.mod_hook_list {
            if !self.guards_ok(&h.guards)? {
                self.out.log.push(format!("Guard condition not met for 'Hook [{}]', skipping.", h.name));
                continue;
            }
            let n = regex::escape(&h.name);
            let re = Regex::new(&format!(r"(?s)({n}\s*[=:]\s*function\s*\(([^)]*)\))|(function\s+{n}\s*\(([^)]*)\))")).unwrap();
            let Some(c) = re.captures(&self.html) else {
                self.fail(format!("Hook: no match for '{}'", h.name), true);
                continue;
            };
            let params = py_strip(c.get(2).or_else(|| c.get(4)).map(|m| m.as_str()).unwrap_or("")).to_string();
            let match_end = c.get(0).unwrap().end();
            let wrapper_var = format!("{}__orig", h.name.replace(['.', '[', ']'], "_"));

            let mut names: Vec<String> = Vec::new();
            if !params.is_empty() {
                for p in params.split(',') {
                    let pn = py_strip(p).trim_start_matches('.').split('=').next().unwrap_or("");
                    let pn = py_strip(pn);
                    if !pn.is_empty() {
                        names.push(pn.to_string());
                    }
                }
            }
            let destructure = if names.is_empty() { String::new() } else { format!("    const [{}] = arguments;\n", names.join(", ")) };
            let body = &h.body;
            let wrapper = match h.timing.as_str() {
                "before" => format!(
                    "var {wrapper_var} = {name};\n{name} = function() {{\n{destructure}    {body}\n    return {wrapper_var}.apply(this, arguments);\n}};\n",
                    name = h.name
                ),
                "after" => format!(
                    "var {wrapper_var} = {name};\n{name} = function() {{\n{destructure}    var _result = {wrapper_var}.apply(this, arguments);\n    {body}\n    return _result;\n}};\n",
                    name = h.name
                ),
                _ => format!(
                    "var {wrapper_var} = {name};\n{name} = function() {{\n{destructure}    var _result = {wrapper_var}.apply(this, arguments);\n    (function() {{ {body} }}).call(this);\n    return _result;\n}};\n",
                    name = h.name
                ),
            };

            if self.html.contains(&wrapper_var) {
                let wv_idx = self.html.ffind(&wrapper_var).unwrap();
                if let Some(wb_open) = self.html[wv_idx..].find('{').map(|i| wv_idx + i) {
                    if let Some(wb_close) = bracket_match(&self.html, wb_open, b'{', b'}') {
                        let extra = format!("\n    // Hook ({}) [{}]\n    {}", h.timing, h.mod_file, body);
                        self.html.insert_str(wb_close, &extra);
                        self.out.log.push(format!("Hook ({}): appended additional body for '{}' from {}", h.timing, h.name, h.mod_file));
                        self.out.replacements_made += 1;
                        continue;
                    }
                }
                self.out.log.push(format!("Hook: already applied for '{}', skipping.", h.name));
                continue;
            }

            let Some(brace_open) = self.html[match_end..].find('{').map(|i| match_end + i) else {
                self.soft_fail(format!("Hook: could not find opening brace for '{}'", h.name));
                continue;
            };
            let Some(brace_close) = bracket_match(&self.html, brace_open, b'{', b'}') else {
                self.soft_fail(format!("Hook: could not find body end for '{}'", h.name));
                continue;
            };
            self.html.insert_str(brace_close + 1, &format!("\n{wrapper}"));
            self.out.log.push(format!("Hook ({}): wrapped '{}'", h.timing, h.name));
            self.out.replacements_made += 1;
        }
        Ok(())
    }

    /// `Replace In All Passages [tag]:`. The Python version writes at positions recorded before the
    /// directive ran, so after the first change the next passages are written to the wrong place; this
    /// version looks every passage up fresh and applies from the last passage to the first.
    pub(super) fn replace_in_all_passages(&mut self, e: &FuncEntry) -> Result<()> {
        let tag = e.name.clone();
        if !self.guards_ok(&e.guards)? {
            self.out.log.push(format!("Guard condition not met for 'Replace In All Passages [{tag}]', skipping."));
            return Ok(());
        }
        let anchor = escape_twine_tags(e.anchor.as_deref().unwrap_or(""));
        let new_body = e.body.clone().unwrap_or_default();
        check_literal_anchor(&anchor)?;
        let snap = split_html(&self.html);
        let mut tagged: Vec<(String, crate::html::PassageMeta)> = Vec::new();
        for (name, meta) in &snap.passage_meta {
            let o = &meta.open_tag;
            if o.contains(&format!(" {tag} ")) || o.contains(&format!("\"{tag}\"")) || o.contains(&format!("\"{tag} ")) || o.contains(&format!(" {tag}\"")) {
                tagged.push((name.clone(), meta.clone()));
            }
        }
        if tagged.is_empty() {
            self.out.log.push(format!("Replace In All Passages [{tag}]: no passages with that tag found"));
            return Ok(());
        }
        let total = tagged.len();
        tagged.sort_by(|a, b| b.1.start.cmp(&a.1.start));
        let mut applied = 0usize;
        for (name, meta) in &tagged {
            let body_start = meta.start + meta.open_tag.len();
            let body_end = meta.end - PASSAGE_CLOSE.len();
            let body = self.html[body_start..body_end].to_string();
            let Some(rel) = body.find(&anchor) else { continue };
            if body.contains(&new_body) {
                self.out.log.push(format!("Replace In All Passages [{tag}]: already applied in '{name}', skipping."));
                continue;
            }
            let abs = body_start + rel;
            self.html.replace_range(abs..abs + anchor.len(), &new_body);
            applied += 1;
        }
        if applied > 0 {
            self.out.log.push(format!("Replace In All Passages [{tag}]: applied to {applied}/{total} passages"));
            self.out.replacements_made += 1;
        } else {
            self.out.log.push(format!("Replace In All Passages [{tag}]: anchor not found in any tagged passage"));
        }
        Ok(())
    }

    /// `Add StoryVar [name]: value` (StoryInit line plus a `setup.saveStateFields` registration).
    pub(super) fn add_storyvar(&mut self, e: &FuncEntry) -> Result<()> {
        if !self.guards_ok(&e.guards)? {
            self.out.log.push(format!("Guard condition not met for 'Add StoryVar [{}]', skipping.", e.name));
            return Ok(());
        }
        let Some(sp) = find_passage(&self.html, "StoryInit") else {
            self.fail(format!("Add StoryVar: StoryInit passage not found for '{}'", e.name), true);
            return Ok(());
        };
        let var_re = Regex::new(&format!(r"\${}\b", regex::escape(&e.name))).unwrap();
        if !var_re.is_match(&self.html[sp.body_start..sp.body_end]) {
            let value = e.value.clone().unwrap_or_else(|| "undefined".to_string());
            self.html.insert_str(sp.body_start, &format!("&lt;&lt;set ${} to {}&gt;&gt;\n", e.name, value));
            self.out.log.push(format!("Add StoryVar: injected '${}' into StoryInit", e.name));
        } else {
            self.out.log.push(format!("Add StoryVar: '{}' already in StoryInit, skipping init.", e.name));
        }
        let check = format!("saveStateFields.indexOf('{}')", e.name);
        if !self.html.contains(&check) {
            let snippet = format!(
                "if (typeof setup.saveStateFields === 'undefined') {{ setup.saveStateFields = []; }}\nif (setup.saveStateFields.indexOf('{n}') === -1) {{ setup.saveStateFields.push('{n}'); }}",
                n = e.name
            );
            self.js_extra.push(BodyRecord { body: snippet, guards: Guards::default(), mod_file: e.mod_file.clone() });
            self.out.log.push(format!("Add StoryVar: registered '{}' in setup.saveStateFields", e.name));
        } else {
            self.out.log.push(format!("Add StoryVar: '{}' already in saveStateFields, skipping.", e.name));
        }
        self.out.replacements_made += 1;
        Ok(())
    }

    /// `Clone Passage [src] As [dst]`: queue a copy of an existing passage (vanilla, or an `Add Passage` one)
    /// under a new name. The source body and tags come from the snapshot taken after the first phase, like
    /// the Python code. The Python version queues the clone with a bare `{}` guards record, which makes the
    /// later `Add Passage` injection raise `KeyError: 'if_exists'`; here the record carries empty guards.
    pub(super) fn clone_passage(&mut self, e: &FuncEntry) -> Result<()> {
        let src = e.name.clone();
        let dst = e.new_name.clone().unwrap_or_default();
        if !self.guards_ok(&e.guards)? {
            self.out.log.push(format!("Guard condition not met for 'Clone Passage [{src}]', skipping."));
            return Ok(());
        }
        let (src_body, src_tags) = if let Some(body) = self.snap.passage_dict.get(&src) {
            let tags = self
                .snap
                .passage_meta
                .get(&src)
                .and_then(|m| Regex::new(r#"tags="([^"]*)"#).unwrap().captures(&m.open_tag).map(|c| c[1].to_string()))
                .unwrap_or_default();
            (body.clone(), tags)
        } else if let Some(&idx) = self.names_seen.get(&src) {
            (self.registry[idx].body.clone(), self.registry[idx].tags.clone())
        } else {
            self.fail(format!("Clone Passage: source '{src}' not found"), true);
            return Ok(());
        };
        if self.snap.passage_dict.contains_key(&dst) {
            self.out.log.push(format!("Clone Passage: destination '{dst}' already exists, skipping."));
            return Ok(());
        }
        if self.names_seen.contains_key(&dst) {
            self.out.log.push(format!("Clone Passage: '{dst}' already in registry, skipping."));
            return Ok(());
        }
        self.names_seen.insert(dst.clone(), self.registry.len());
        self.registry.push(PassageRecord {
            name: dst.clone(),
            tags: src_tags,
            pid: "auto".to_string(),
            body: src_body,
            guards: PassageGuards::Full(Guards::default()),
            mod_file: e.mod_file.clone(),
            format: "full".to_string(),
        });
        self.out.log.push(format!("Clone Passage: queued '{src}' -> '{dst}' for Phase 7 injection"));
        self.out.replacements_made += 1;
        Ok(())
    }
}
