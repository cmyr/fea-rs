//use fea_rs::{Source, };
use criterion::{black_box, criterion_group, criterion_main, Criterion};

//const INPUT2: &str = include_str!("/Users/rofls/dev/projects/fontville/test-ufo-data/plex/IBM-Plex-Sans-Devanagari/sources/masters/IBM Plex Sans Devanagari-Regular.ufo/features.fea");
const INPUT2: &str = include_str!("/Users/rofls/downloads/features_simon.fea");

fn parse_me(source: &fea_rs::Source) -> fea_rs::Node {
    fea_rs::parse_src(source, None).0
}

fn criterion_benchmark(c: &mut Criterion) {
    let source = fea_rs::Source::from_text(INPUT2);
    c.bench_function("parse something", |b| {
        b.iter(|| parse_me(black_box(&source)))
    });
}

criterion_group!(benches, criterion_benchmark);
criterion_main!(benches);
