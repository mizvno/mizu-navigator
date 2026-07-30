fn main() {
    let entry = keyring::Entry::new("mizunavigator", "historykey").unwrap();
    println!("Get 1: {:?}", entry.get_password());
    entry.set_password("my-secret").unwrap();
    println!("Get 2: {:?}", entry.get_password());
}
