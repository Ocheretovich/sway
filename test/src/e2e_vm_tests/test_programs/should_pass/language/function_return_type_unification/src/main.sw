script;

trait Build {
    fn build() -> Self;
}

impl Build for u32 {
    fn build() -> Self {
        31
    }
}

impl Build for u64 {
    fn build() -> Self {
        63
    }
}

fn produce<T>() -> T
where T: Build,
{
    T::build()
}

fn main() -> bool {
    let _:u32 = produce();

    true
}