//! Small helper binary used by Windows sandbox integration tests.
//!
//! It exits with code:
//! - 0 when `FASTR_SECRET_SHOULD_NOT_LEAK` is NOT present
//! - 1 when the variable IS present

fn main() {
  const SECRET_ENV: &str = "FASTR_SECRET_SHOULD_NOT_LEAK";

  if std::env::var_os(SECRET_ENV).is_some() {
    println!("{SECRET_ENV}=<present>");
    std::process::exit(1);
  }

  println!("{SECRET_ENV}=<absent>");
}

