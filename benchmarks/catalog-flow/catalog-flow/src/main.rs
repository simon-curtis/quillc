fn main() {
    let (records, digest) = catalog_flow::run();
    println!("catalog-flow records={records} digest={digest:016x}");
}
