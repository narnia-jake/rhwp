//! 메모(주석) 목록·추가·고치기·지우기. 모델 쪽 규약은 [`crate::model::memo`].
//!
//! - 앵커(필드)는 본문 문단에, 메모 글은 필드의 `memo_paragraphs` 에 있다.
//! - 저장할 때 메모 글은 마지막 구역 끝으로 모이므로, 메모를 바꾸면 앵커가 있는 구역과
//!   마지막 구역의 `raw_stream` 을 함께 비워 모델에서 다시 쓰게 한다.

use super::field_query::{
    collect_max_field_id, field_location_json, field_range_bounds, rebuild_char_offsets,
};
use crate::document_core::helpers::json_escape;
use crate::document_core::DocumentCore;
use crate::error::HwpError;
use crate::model::control::{Control, Field, FieldType};
use crate::model::event::DocumentEvent;
use crate::model::memo;
use crate::model::paragraph::{CharShapeRef, FieldRange, Paragraph};
use crate::parser::tags;

/// 1601-01-01 ~ 1970-01-01, 100ns 단위.
const FILETIME_UNIX_EPOCH: u64 = 116_444_736_000_000_000;

pub fn filetime_from_unix_ms(ms: f64) -> u64 {
    if !ms.is_finite() || ms < 0.0 {
        return FILETIME_UNIX_EPOCH;
    }
    FILETIME_UNIX_EPOCH + (ms as u64) * 10_000
}

fn unix_ms_from_filetime(ft: u64) -> Option<u64> {
    ft.checked_sub(FILETIME_UNIX_EPOCH).map(|d| d / 10_000)
}

impl DocumentCore {
    /// 메모 목록 JSON.
    ///
    /// `[{"id","text","author","createdAt"(unix ms|null),"location"(getFieldList 와 같은 꼴)|null,
    ///    "startCharIdx","endCharIdx","anchorText"}]` — 메모 번호 순.
    pub fn get_memo_list_json(&self) -> String {
        let infos = self.collect_all_fields();
        let mut memos: Vec<&Field> = memo::collect_memo_fields(&self.document.sections);
        memos.sort_by_key(|f| memo::memo_id(f));
        let entries: Vec<String> = memos
            .iter()
            .map(|field| {
                let id = memo::memo_id(field);
                let info = infos
                    .iter()
                    .find(|fi| memo::is_memo_field(&fi.field) && memo::memo_id(&fi.field) == id);
                let author = memo::memo_author(field)
                    .map(|a| format!("\"{}\"", json_escape(a)))
                    .unwrap_or_else(|| "null".into());
                let created = memo::memo_filetime(field)
                    .and_then(unix_ms_from_filetime)
                    .map(|ms| ms.to_string())
                    .unwrap_or_else(|| "null".into());
                let (location, start, end, anchor) = match info {
                    Some(fi) => {
                        let (s, e) = field_range_bounds(self, fi).unwrap_or((0, 0));
                        (field_location_json(&fi.location), s, e, fi.value.as_str())
                    }
                    None => ("null".into(), 0, 0, ""),
                };
                format!(
                    "{{\"id\":{},\"text\":\"{}\",\"author\":{},\"createdAt\":{},\"location\":{},\"startCharIdx\":{},\"endCharIdx\":{},\"anchorText\":\"{}\"}}",
                    id,
                    json_escape(&memo::memo_plain_text(&field.memo_paragraphs)),
                    author,
                    created,
                    location,
                    start,
                    end,
                    json_escape(anchor),
                )
            })
            .collect();
        format!("[{}]", entries.join(","))
    }

    /// 한 문단 안의 글자 범위 `[start, end)` 에 메모를 단다.
    ///
    /// `path` 가 비면 본문 문단 `para_idx`, 아니면 그 문단 안 표 셀 경로
    /// (control, cell, cell_para) 의 문단. 반환 `{"ok":true,"id":N}`.
    #[allow(clippy::too_many_arguments)]
    pub fn insert_memo(
        &mut self,
        section_idx: usize,
        para_idx: usize,
        path: &[(usize, usize, usize)],
        start: usize,
        end: usize,
        text: &str,
        author: &str,
        filetime: u64,
    ) -> Result<String, HwpError> {
        if section_idx >= self.document.sections.len() {
            return Err(HwpError::InvalidField("구역 인덱스 초과".into()));
        }
        let id = self.next_memo_id();
        let field_id = {
            let mut max_id = 0u32;
            for section in &self.document.sections {
                for para in &section.paragraphs {
                    collect_max_field_id(para, &mut max_id);
                }
            }
            max_id.saturating_add(1).max(1)
        };
        let field = Field {
            field_type: FieldType::Memo,
            command: memo::build_memo_command(id, filetime, author),
            // 한컴 저장본 값 (비트 15 = 메모)
            properties: 0x8001,
            extra_properties: 0,
            field_id,
            ctrl_id: tags::FIELD_MEMO,
            memo_index: id,
            memo_paragraphs: self.memo_body(text, None),
            ..Default::default()
        };

        let para = if path.is_empty() {
            self.document.sections[section_idx]
                .paragraphs
                .get_mut(para_idx)
                .ok_or_else(|| HwpError::InvalidField("문단 인덱스 초과".into()))?
        } else {
            self.get_cell_paragraph_mut_by_path(section_idx, para_idx, path)?
        };
        let (start, end) = insert_field_range(para, start, end, field);

        if let Some(&(outer_ctrl, _, _)) = path.first() {
            self.mark_cell_control_dirty(section_idx, para_idx, outer_ctrl);
        }
        self.after_memo_change(Some(section_idx));
        self.event_log.push(DocumentEvent::TextInserted {
            section: section_idx,
            para: para_idx,
            offset: start,
            len: 0,
        });
        Ok(format!(
            "{{\"ok\":true,\"id\":{},\"startCharIdx\":{},\"endCharIdx\":{}}}",
            id, start, end
        ))
    }

    /// 메모 글을 바꾼다. 반환 `{"ok":true}`.
    pub fn update_memo(&mut self, id: u32, text: &str) -> Result<String, HwpError> {
        let template = memo::collect_memo_fields(&self.document.sections)
            .into_iter()
            .find(|f| memo::memo_id(f) == id)
            .map(|f| f.memo_paragraphs.first().cloned())
            .ok_or_else(|| HwpError::InvalidField(format!("메모 {id} 없음")))?;
        let body = self.memo_body(text, template.as_ref());
        let mut body = Some(body);
        memo::for_each_field_mut(&mut self.document.sections, &mut |field| {
            if memo::is_memo_field(field) && memo::memo_id(field) == id {
                if let Some(b) = body.take() {
                    field.memo_paragraphs = b;
                }
            }
        });
        // 글 자체는 화면에 그리지 않으므로 다시 조판할 것은 없다.
        self.after_memo_change(None);
        Ok(r#"{"ok":true}"#.to_string())
    }

    /// 메모를 지운다. 걸려 있던 본문 글자는 그대로 둔다. 반환 `{"ok":true}`.
    pub fn delete_memo(&mut self, id: u32) -> Result<String, HwpError> {
        let mut hit: Option<usize> = None;
        memo::for_each_paragraph_mut(&mut self.document.sections, &mut |si, para| {
            if hit.is_some() {
                return;
            }
            let found = para.controls.iter().position(|c| {
                matches!(c, Control::Field(f) if memo::is_memo_field(f) && memo::memo_id(f) == id)
            });
            if let Some(ci) = found {
                remove_field_control(para, ci);
                hit = Some(si);
            }
        });
        let mut orphan = false;
        for section in &mut self.document.sections {
            let before = section.memo_lists.len();
            section.memo_lists.retain(|(mid, _)| *mid != id);
            orphan |= section.memo_lists.len() != before;
        }
        if hit.is_none() && !orphan {
            return Err(HwpError::InvalidField(format!("메모 {id} 없음")));
        }
        self.after_memo_change(hit);
        Ok(r#"{"ok":true}"#.to_string())
    }

    fn next_memo_id(&self) -> u32 {
        let fields = memo::collect_memo_fields(&self.document.sections)
            .into_iter()
            .map(memo::memo_id);
        let orphans = self
            .document
            .sections
            .iter()
            .flat_map(|s| s.memo_lists.iter().map(|(id, _)| *id));
        fields
            .chain(orphans)
            .max()
            .unwrap_or(0)
            .saturating_add(1)
            .max(1)
    }

    /// 메모 글 문단들. 서식은 `template` → 문서의 다른 메모 → '메모' 스타일 → 0번 순으로.
    fn memo_body(&self, text: &str, template: Option<&Paragraph>) -> Vec<Paragraph> {
        let existing;
        let template = match template {
            Some(t) => Some(t),
            None => {
                existing = memo::collect_memo_fields(&self.document.sections)
                    .into_iter()
                    .find_map(|f| f.memo_paragraphs.first().cloned());
                existing.as_ref()
            }
        };
        let (para_shape_id, style_id, char_shape_id) = template
            .map(|p| {
                (
                    p.para_shape_id,
                    p.style_id,
                    p.char_shapes.first().map_or(0, |c| c.char_shape_id),
                )
            })
            .or_else(|| {
                let styles = &self.document.doc_info.styles;
                styles.iter().position(|s| s.local_name == "메모").map(|i| {
                    (
                        styles[i].para_shape_id,
                        i as u8,
                        styles[i].char_shape_id as u32,
                    )
                })
            })
            .unwrap_or((0, 0, 0));

        let text = text
            .replace("\r\n", "\n")
            .replace('\r', "\n")
            .replace('\t', " ");
        text.split('\n')
            .map(|line| {
                let line: String = line.chars().filter(|c| (*c as u32) >= 0x20).collect();
                let mut para = Paragraph::new_empty();
                para.para_shape_id = para_shape_id;
                para.style_id = style_id;
                para.char_shapes = vec![CharShapeRef {
                    start_pos: 0,
                    char_shape_id,
                }];
                let mut offsets = Vec::new();
                let mut unit = 0u32;
                for ch in line.chars() {
                    offsets.push(unit);
                    unit += ch.len_utf16() as u32;
                }
                para.char_offsets = offsets;
                para.char_count = unit + 1;
                para.has_para_text = !line.is_empty();
                para.text = line;
                para
            })
            .collect()
    }

    /// 메모를 바꾼 뒤: 저장 때 모델에서 다시 쓰도록 원본 스트림을 버리고, 앵커 구역을 다시 짠다.
    fn after_memo_change(&mut self, anchor_section: Option<usize>) {
        let last = self.document.sections.len().saturating_sub(1);
        for si in [anchor_section, Some(last)].into_iter().flatten() {
            if let Some(section) = self.document.sections.get_mut(si) {
                section.raw_stream = None;
            }
        }
        if let Some(si) = anchor_section {
            self.recompose_section(si);
            self.paginate_if_needed();
        }
        self.invalidate_page_tree_cache();
    }
}

/// 필드 컨트롤을 `[start, end)` 에 넣는다. 범위는 문단 글자 수로 자른다.
fn insert_field_range(
    para: &mut Paragraph,
    start: usize,
    end: usize,
    field: Field,
) -> (usize, usize) {
    let text_len = para.text.chars().count();
    let start = start.min(text_len);
    let end = end.clamp(start, text_len);
    let insert_idx = para
        .control_text_positions()
        .iter()
        .position(|&pos| pos > start)
        .unwrap_or(para.controls.len());

    for range in &mut para.field_ranges {
        if range.control_idx >= insert_idx {
            range.control_idx += 1;
        }
    }
    para.align_ctrl_data_records();
    para.controls.insert(insert_idx, Control::Field(field));
    para.ctrl_data_records.insert(insert_idx, None);

    let range_idx = para
        .field_ranges
        .iter()
        .position(|r| {
            r.start_char_idx > start || (r.start_char_idx == start && r.control_idx > insert_idx)
        })
        .unwrap_or(para.field_ranges.len());
    para.field_ranges.insert(
        range_idx,
        FieldRange {
            start_char_idx: start,
            end_char_idx: end,
            control_idx: insert_idx,
            end_field_id: 0,
        },
    );
    rebuild_char_offsets(para);
    (start, end)
}

/// 필드 컨트롤과 그 범위만 뺀다 (글자는 남긴다).
fn remove_field_control(para: &mut Paragraph, control_idx: usize) {
    para.field_ranges.retain(|r| r.control_idx != control_idx);
    para.controls.remove(control_idx);
    if control_idx < para.ctrl_data_records.len() {
        para.ctrl_data_records.remove(control_idx);
    }
    for range in &mut para.field_ranges {
        if range.control_idx > control_idx {
            range.control_idx -= 1;
        }
    }
    rebuild_char_offsets(para);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn core_with_text(text: &str) -> DocumentCore {
        let mut core = DocumentCore::new_empty();
        core.create_blank_document_native().unwrap();
        core.insert_text_native(0, 0, 0, text).unwrap();
        core
    }

    fn list(core: &DocumentCore) -> serde_json::Value {
        serde_json::from_str(&core.get_memo_list_json()).unwrap()
    }

    #[test]
    fn insert_update_delete_round_trip() {
        let mut core = core_with_text("가나다라마바사");
        let ft = filetime_from_unix_ms(1_760_000_000_000.0);
        let r = core
            .insert_memo(0, 0, &[], 2, 4, "첫 메모\n둘째 줄", "검토자", ft)
            .unwrap();
        assert!(r.contains("\"id\":1"), "{r}");
        core.insert_memo(0, 0, &[], 5, 6, "다른 메모", "", ft)
            .unwrap();

        let v = list(&core);
        assert_eq!(v.as_array().unwrap().len(), 2);
        assert_eq!(v[0]["text"], "첫 메모\n둘째 줄");
        assert_eq!(v[0]["author"], "검토자");
        assert_eq!(v[0]["anchorText"], "다라");
        assert_eq!(v[0]["createdAt"], 1_760_000_000_000u64);
        assert_eq!(v[1]["id"], 2);
        assert!(v[1]["author"].is_null());

        // 저장 → 다시 읽기
        let bytes = core.export_hwp_native().unwrap();
        let mut again = DocumentCore::from_bytes(&bytes).unwrap();
        let v = list(&again);
        assert_eq!(v[0]["text"], "첫 메모\n둘째 줄");
        assert_eq!(v[0]["anchorText"], "다라");
        assert_eq!(v[1]["text"], "다른 메모");
        assert_eq!(
            again.document.sections[0].paragraphs[0].text,
            "가나다라마바사"
        );

        again.update_memo(1, "고친 메모").unwrap();
        again.delete_memo(2).unwrap();
        assert!(again.delete_memo(99).is_err());
        let bytes = again.export_hwp_native().unwrap();
        let third = DocumentCore::from_bytes(&bytes).unwrap();
        let v = list(&third);
        assert_eq!(v.as_array().unwrap().len(), 1);
        assert_eq!(v[0]["text"], "고친 메모");
        assert_eq!(
            third.document.sections[0].paragraphs[0].text,
            "가나다라마바사"
        );
        assert_eq!(
            third.document.sections[0].paragraphs.len(),
            1,
            "메모 뿌리 문단은 모델에 남지 않는다"
        );
    }

    #[test]
    fn filetime_conversion() {
        assert_eq!(
            unix_ms_from_filetime(filetime_from_unix_ms(1234.0)),
            Some(1234)
        );
        assert_eq!(filetime_from_unix_ms(f64::NAN), FILETIME_UNIX_EPOCH);
    }
}
