//! M8's exit check, first half: a golden set of drops with made-up names
//! gives exactly the expected plans (`drops_golden.json`). Each case lists
//! the dropped paths, any metadata files' text, where it was dropped, and
//! the plan: every job without its size, then the files refused (with a
//! reason) and ignored.

use koma::drops::{self, Drop, Known, Place, Target};
use koma::names::Kind;
use serde_json::{Value, json};

const GOLDEN: &str = include_str!("drops_golden.json");

#[test]
fn golden_drops_give_the_expected_plans() {
    let cases: Value = serde_json::from_str(GOLDEN).expect("golden JSON");
    let cases = cases.as_array().expect("a list");
    assert!(cases.len() >= 20);
    let mut failures = Vec::new();
    for case in cases {
        let name = case["drop"].as_str().expect("a name");
        let items: Vec<Value> = case["items"]
            .as_array()
            .expect("items")
            .iter()
            .map(|p| json!({"path": p, "size": 10}))
            .collect();
        let sent = json!({"items": items, "meta": case.get("meta").cloned().unwrap_or(json!({})), "today": "2026-01-01"});
        let target = match (case.get("series"), case.get("collection")) {
            (Some(s), _) => Target::Series(s.as_i64().expect("a series id")),
            (None, Some(c)) => Target::Collection(c.as_str().expect("a name").to_owned()),
            (None, None) => Target::Library,
        };
        let cosplay = case
            .get("cosplay")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let drop = Drop::from_json(&sent, target, cosplay).expect("a drop");
        let plan = drops::plan(&drop);
        let got: Vec<Value> = plan
            .jobs
            .iter()
            .map(|j| {
                let mut v = j.to_json();
                v.as_object_mut().expect("object").remove("size");
                v
            })
            .collect();
        let refused: Vec<usize> = plan.refused.iter().map(|(i, _)| *i).collect();
        let want_refused: Vec<usize> =
            serde_json::from_value(case.get("refused").cloned().unwrap_or(json!([])))
                .expect("list");
        let want_ignored: Vec<usize> =
            serde_json::from_value(case.get("ignored").cloned().unwrap_or(json!([])))
                .expect("list");
        if got != *case["plan"].as_array().expect("plan")
            || refused != want_refused
            || plan.ignored != want_ignored
        {
            failures.push(format!(
                "{name}:\n  got     {}\n  refused {refused:?} ignored {:?}",
                serde_json::to_string(&got).expect("json"),
                plan.ignored
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn sizes_add_up_and_cbr_says_why() {
    let sent = json!({"items": [
        {"path": "Starfall/001.jpg", "size": 300},
        {"path": "Starfall/002.jpg", "size": 200},
        {"path": "Starfall/ComicInfo.xml", "size": 5},
        {"path": "old.cbr", "size": 9}
    ]});
    let plan = drops::plan(&Drop::from_json(&sent, Target::Library, false).expect("a drop"));
    assert_eq!(plan.jobs.len(), 1);
    assert_eq!(plan.jobs[0].size, 505);
    assert_eq!(plan.refused.len(), 1);
    assert!(plan.refused[0].1.contains("M11"));
}

#[test]
fn bad_drops_are_refused() {
    assert!(Drop::from_json(&json!({}), Target::Library, false).is_err());
    assert!(Drop::from_json(&json!({"items": [{"size": 1}]}), Target::Library, false).is_err());
    let many: Vec<Value> = (0..drops::MAX_ITEMS + 1)
        .map(|i| json!({"path": format!("{i}.jpg")}))
        .collect();
    assert!(Drop::from_json(&json!({"items": many}), Target::Library, false).is_err());
}

fn planned(
    sent: Value,
    target: Target,
    cosplay: bool,
    known: &dyn Fn(&Place) -> Known,
) -> Vec<(String, Option<String>)> {
    let drop = Drop::from_json(&sent, target, cosplay).expect("a drop");
    let mut plan = drops::plan(&drop);
    drops::name_to_standard(&mut plan, &drop, known);
    plan.jobs
        .iter()
        .map(|j| (j.landing_name(true), j.note.clone()))
        .collect()
}

fn paths(list: &[&str]) -> Value {
    Value::Array(list.iter().map(|p| json!({"path": p, "size": 1})).collect())
}

#[test]
fn new_series_get_standard_names() {
    let sent = json!({
        "items": paths(&["Starfall Academy/ch 1/001.jpg", "Starfall Academy/ch 2/001.jpg",
                         "Starfall Academy/ch 2/metadata.json", "Starfall Academy/ch 10/001.jpg"]),
        "meta": {"2": "{\"artists\": [\"Mirelle\"], \"groups\": [\"Moonforge\"], \"language\": \"English\"}"},
    });
    let got = planned(sent, Target::Library, false, &|_| Known::default());
    assert_eq!(
        got,
        vec![
            (
                "[Moonforge (Mirelle)] Starfall Academy - Ch. 01 [EN].cbz".to_owned(),
                None
            ),
            (
                "[Moonforge (Mirelle)] Starfall Academy - Ch. 02 [EN].cbz".to_owned(),
                None
            ),
            (
                "[Moonforge (Mirelle)] Starfall Academy - Ch. 10 [EN].cbz".to_owned(),
                None
            ),
        ]
    );
}

#[test]
fn a_known_series_lends_its_name_author_and_padding() {
    let known = |p: &Place| match p {
        Place::Series(12) => Known {
            series: Some("Moonforge".into()),
            author: Some("Mirelle".into()),
            circle: Some("Lumen Studio".into()),
            language: Some("EN".into()),
            numbers: vec![(Some(Kind::Chapter), 120.0)],
            books: 30,
        },
        _ => Known::default(),
    };
    let sent =
        json!({"items": paths(&["moonforge_c7_[eng].cb7", "extras.cbz", "Moonforge v2.epub"])});
    let mut got = planned(sent, Target::Series(12), false, &known);
    got.sort();
    assert_eq!(
        got,
        vec![
            (
                "[Lumen Studio (Mirelle)] Moonforge - Ch. 007 [EN].cbz".to_owned(),
                None
            ),
            (
                "[Lumen Studio (Mirelle)] Moonforge - Vol. 2 [EN].epub".to_owned(),
                None
            ),
            (
                "extras.cbz".to_owned(),
                Some("no number, and not the series' only book".to_owned())
            ),
        ]
    );
}

#[test]
fn cosplay_sets_get_names_the_tidy_agrees_with() {
    let sent = json!({"items": paths(&[
        "[Lumen Studio] Mirelle - Starfall Bunny - Bonus/001.jpg",
        "Mirelle/Starfall Bunny/Normal/001.jpg",
        "Mirelle/Moon Maid.zip",
        "Solene - Lucky Bunny/Normal.zip",
        "Solene - Lucky Bunny/Bonus.zip",
        "Harbor Sunset/001.jpg",
    ])});
    let drop = Drop::from_json(&sent, Target::Library, true).expect("a drop");
    let mut plan = drops::plan(&drop);
    drops::name_to_standard(&mut plan, &drop, &|_| Known::default());
    let mut named = Vec::new();
    for j in &plan.jobs {
        let Place::Folder { collection, folder } = &j.place else {
            panic!("a folder")
        };
        match &j.to {
            Some(to) => {
                // The tidy reads the same cosplayer and set back from the name.
                let n = koma::names::cosplay_name(to.rsplit_once('.').expect("ext").0);
                assert_eq!(Some(&n.cosplayer), collection.as_ref(), "{to}");
                assert_eq!(Some(&n.set), folder.as_ref(), "{to}");
                named.push(to.clone());
            }
            None => assert!(j.note.as_deref().unwrap_or("").contains("no cosplayer")),
        }
    }
    named.sort();
    assert_eq!(
        named,
        [
            "Mirelle - Moon Maid.zip",
            "Mirelle - Starfall Bunny - Normal.zip",
            "Solene - Lucky Bunny - Bonus.zip",
            "Solene - Lucky Bunny - Normal.zip",
            "[Lumen Studio] Mirelle - Starfall Bunny - Bonus.zip",
        ]
    );
}

#[test]
fn dropped_names_are_kept_on_request_but_cb7_lands_as_cbz() {
    let sent = json!({"items": paths(&["Starfall/sa_c03.cb7"])});
    let drop = Drop::from_json(&sent, Target::Library, false).expect("a drop");
    let mut plan = drops::plan(&drop);
    drops::name_to_standard(&mut plan, &drop, &|_| Known::default());
    assert_eq!(plan.jobs[0].landing_name(false), "sa_c03.cbz");
    assert_eq!(plan.jobs[0].landing_name(true), "Starfall - Ch. 3.cbz");
}
