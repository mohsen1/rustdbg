//! A tiny RPN calculator: parse a space-separated expression, then evaluate it.

#[derive(Debug, Clone, Copy)]
enum Tok {
    Num(i64),
    Add,
    Sub,
    Mul,
    Div,
}

fn parse(src: &str) -> Vec<Tok> {
    src.split_whitespace()
        .map(|w| match w {
            "+" => Tok::Add,
            "-" => Tok::Sub,
            "*" => Tok::Mul,
            "/" => Tok::Div,
            n => Tok::Num(n.parse().expect("number")),
        })
        .collect()
}

fn eval(tokens: &[Tok]) -> i64 {
    let mut stack: Vec<i64> = Vec::new();
    for &t in tokens {
        match t {
            Tok::Num(n) => stack.push(n),
            Tok::Add => {
                let b = stack.pop().unwrap();
                let a = stack.pop().unwrap();
                stack.push(a + b);
            }
            Tok::Sub => {
                let b = stack.pop().unwrap();
                let a = stack.pop().unwrap();
                stack.push(b - a);
            }
            Tok::Mul => {
                let b = stack.pop().unwrap();
                let a = stack.pop().unwrap();
                stack.push(a * b);
            }
            Tok::Div => {
                let b = stack.pop().unwrap();
                let a = stack.pop().unwrap();
                stack.push(b / a);
            }
        }
    }
    stack.pop().unwrap()
}

/// Evaluate an RPN expression such as `"10 3 - 2 * 1 +"`.
pub fn calc(src: &str) -> i64 {
    eval(&parse(src))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evaluates_expressions() {
        // "10 3 - 2 * 1 +"  ==  (10 - 3) * 2 + 1  ==  15
        assert_eq!(calc("10 3 - 2 * 1 +"), 15);
        // "100 5 / 3 -"     ==  (100 / 5) - 3     ==  17
        assert_eq!(calc("100 5 / 3 -"), 17);
    }
}
