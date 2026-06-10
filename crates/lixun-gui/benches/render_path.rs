use criterion::{Criterion, black_box, criterion_group, criterion_main};
use lixun_core::{Action, Category, DocId, Hit};
use std::time::Duration;

fn make_hits(count: usize) -> Vec<Hit> {
    (0..count)
        .map(|i| Hit {
            id: DocId(format!("doc-{i}")),
            category: Category::File,
            title: format!("Result {i}"),
            subtitle: "subtitle".to_string(),
            icon_name: Some("text-x-generic".to_string()),
            kind_label: Some("File".to_string()),
            score: (count - i) as f32,
            action: Action::Exec {
                cmdline: vec!["true".to_string()],
                working_dir: None,
                terminal: false,
            },
            extract_fail: false,
            sender: None,
            recipients: None,
            body: None,
            secondary_action: None,
            source_instance: "fs".to_string(),
            row_menu: lixun_core::RowMenuDef::empty(),
            mime: None,
        })
        .collect()
}

/// Mix of categories with theme-resolvable icon names, exercising
/// the A5 main-thread paintable cache: hits 0..6 each declare a
/// distinct icon name, hits 6..30 cycle through the same set so
/// the cache must amortise lookups across rows. Absolute-path
/// icons are NOT used here because the bench runs without a real
/// display in some environments; the cross-thread texture cache
/// is still exercised by the icon-name path through `resolve_icon`
/// which consults the main-thread paintable cache on every call.
fn make_hits_with_icons(count: usize) -> Vec<Hit> {
    const ICON_NAMES: &[(&str, Category)] = &[
        ("text-x-generic", Category::File),
        ("application-x-executable", Category::App),
        ("mail-message", Category::Mail),
        ("mail-attachment", Category::Attachment),
        ("accessories-calculator", Category::Calculator),
        ("utilities-terminal", Category::Shell),
    ];
    (0..count)
        .map(|i| {
            let (icon, category) = ICON_NAMES[i % ICON_NAMES.len()];
            Hit {
                id: DocId(format!("doc-icon-{i}")),
                category,
                title: format!("Iconified result {i}"),
                subtitle: format!("/path/to/result-{i}"),
                icon_name: Some(icon.to_string()),
                kind_label: Some("File".to_string()),
                score: (count - i) as f32,
                action: Action::Exec {
                    cmdline: vec!["true".to_string()],
                    working_dir: None,
                    terminal: false,
                },
                extract_fail: false,
                sender: None,
                recipients: None,
                body: None,
                secondary_action: None,
                source_instance: "fs".to_string(),
                row_menu: lixun_core::RowMenuDef::empty(),
                mime: None,
            }
        })
        .collect()
}

fn serialize_response(hits: &[Hit]) -> Vec<u8> {
    let chunk = lixun_ipc::Response::SearchChunk {
        epoch: 1,
        phase: lixun_ipc::Phase::Final,
        hits: hits.to_vec(),
        calculation: None,
        top_hit: None,
        explanations: Vec::new(),
        claimed: false,
    };
    serde_json::to_vec(&chunk).unwrap()
}

fn deserialize_response(bytes: &[u8]) -> lixun_ipc::Response {
    serde_json::from_slice(bytes).unwrap()
}

fn bench_ipc_receive_to_model_insertion(c: &mut Criterion) {
    let mut group = c.benchmark_group("render_path");
    group.sample_size(50);
    group.measurement_time(Duration::from_secs(30));

    let hits_10 = make_hits(10);
    let hits_30 = make_hits(30);
    let bytes_10 = serialize_response(&hits_10);
    let bytes_30 = serialize_response(&hits_30);

    let gtk_ready = gtk::init().is_ok();

    group.bench_function("ipc-receive-10hits", |b| {
        b.iter(|| {
            let resp = deserialize_response(&bytes_10);
            if let lixun_ipc::Response::SearchChunk { hits, top_hit, .. } = resp {
                let plan = lixun_gui::compute_render_plan(&hits, top_hit.as_ref());
                if gtk_ready {
                    let model = gtk::StringList::new(&[]);
                    let selection = gtk::SingleSelection::builder().model(&model).build();
                    lixun_gui::update_results(
                        &model,
                        &selection,
                        &plan.hits,
                        plan.top_hit_index
                            .and_then(|i| plan.hits.get(i))
                            .map(|h| h.id.0.clone()),
                    );
                    black_box(&model);
                } else {
                    black_box(plan);
                }
            }
        });
    });

    group.bench_function("ipc-receive-30hits", |b| {
        b.iter(|| {
            let resp = deserialize_response(&bytes_30);
            if let lixun_ipc::Response::SearchChunk { hits, top_hit, .. } = resp {
                let plan = lixun_gui::compute_render_plan(&hits, top_hit.as_ref());
                if gtk_ready {
                    let model = gtk::StringList::new(&[]);
                    let selection = gtk::SingleSelection::builder().model(&model).build();
                    lixun_gui::update_results(
                        &model,
                        &selection,
                        &plan.hits,
                        plan.top_hit_index
                            .and_then(|i| plan.hits.get(i))
                            .map(|h| h.id.0.clone()),
                    );
                    black_box(&model);
                } else {
                    black_box(plan);
                }
            }
        });
    });

    let hits_30_icons = make_hits_with_icons(30);
    let bytes_30_icons = serialize_response(&hits_30_icons);

    group.bench_function("render-30hits-with-icons", |b| {
        b.iter(|| {
            let resp = deserialize_response(&bytes_30_icons);
            if let lixun_ipc::Response::SearchChunk { hits, top_hit, .. } = resp {
                let plan = lixun_gui::compute_render_plan(&hits, top_hit.as_ref());
                if gtk_ready {
                    let model = gtk::StringList::new(&[]);
                    let selection = gtk::SingleSelection::builder().model(&model).build();
                    lixun_gui::update_results(
                        &model,
                        &selection,
                        &plan.hits,
                        plan.top_hit_index
                            .and_then(|i| plan.hits.get(i))
                            .map(|h| h.id.0.clone()),
                    );
                    black_box(&model);
                } else {
                    black_box(plan);
                }
            }
        });
    });

    group.finish();
}

criterion_group!(benches, bench_ipc_receive_to_model_insertion);
criterion_main!(benches);
