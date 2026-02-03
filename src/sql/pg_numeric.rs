use rust_decimal::Decimal;
use rust_decimal::RoundingStrategy;

const PG_NUMERIC_MIN_SIG_DIGITS: i32 = 16;
const PG_NUMERIC_DEC_DIGITS: i32 = 4;
const PG_NUMERIC_MIN_DISPLAY_SCALE: i32 = 0;
const PG_NUMERIC_MAX_DISPLAY_SCALE: i32 = 28; // rust_decimal max scale

fn pg_weight_and_firstdigit(d: &Decimal) -> (i32, i32) {
    if d.is_zero() {
        return (0, 0);
    }

    let abs = d.abs();
    let unpacked = abs.unpack();
    let scale = unpacked.scale as usize;
    let mantissa: u128 =
        (unpacked.hi as u128) << 64 | (unpacked.mid as u128) << 32 | (unpacked.lo as u128);

    if mantissa == 0 {
        return (0, 0);
    }

    let digits = mantissa.to_string();
    let (int_part, frac_part) = if scale == 0 {
        (digits.clone(), String::new())
    } else if digits.len() > scale {
        (
            digits[..digits.len() - scale].to_string(),
            digits[digits.len() - scale..].to_string(),
        )
    } else {
        (String::new(), format!("{:0>width$}", digits, width = scale))
    };

    let group_digits = PG_NUMERIC_DEC_DIGITS as usize;
    let mut groups: Vec<i32> = Vec::new();

    let mut int_groups: Vec<i32> = Vec::new();
    if !int_part.is_empty() {
        let mut end = int_part.len();
        while end > 0 {
            let start = end.saturating_sub(group_digits);
            let chunk = &int_part[start..end];
            int_groups.push(chunk.parse::<i32>().unwrap_or(0));
            end = start;
        }
        int_groups.reverse();
        groups.extend(int_groups.iter().copied());
    }

    if !frac_part.is_empty() {
        let mut start = 0;
        while start < frac_part.len() {
            let end = (start + group_digits).min(frac_part.len());
            let chunk = &frac_part[start..end];
            let mut buf = chunk.to_string();
            if buf.len() < group_digits {
                buf.push_str(&"0".repeat(group_digits - buf.len()));
            }
            groups.push(buf.parse::<i32>().unwrap_or(0));
            start = end;
        }
    }

    let weight0 = if int_groups.is_empty() {
        -1
    } else {
        int_groups.len() as i32 - 1
    };

    for (idx, digit) in groups.iter().enumerate() {
        if *digit != 0 {
            return (weight0 - idx as i32, *digit);
        }
    }

    (0, 0)
}

pub(crate) fn pg_select_div_scale(numer: &Decimal, denom: &Decimal) -> u32 {
    let (weight1, firstdigit1) = pg_weight_and_firstdigit(numer);
    let (weight2, firstdigit2) = pg_weight_and_firstdigit(denom);

    let mut qweight = weight1 - weight2;
    if firstdigit1 <= firstdigit2 {
        qweight -= 1;
    }

    let mut scale = PG_NUMERIC_MIN_SIG_DIGITS - qweight * PG_NUMERIC_DEC_DIGITS;
    scale = scale.max(numer.scale() as i32);
    scale = scale.max(denom.scale() as i32);
    scale = scale.max(PG_NUMERIC_MIN_DISPLAY_SCALE);
    scale = scale.min(PG_NUMERIC_MAX_DISPLAY_SCALE);
    scale.max(0) as u32
}

pub(crate) fn pg_numeric_div(numer: Decimal, denom: Decimal) -> Decimal {
    let scale = pg_select_div_scale(&numer, &denom);
    let mut result = numer / denom;
    if result.scale() > scale {
        result = result.round_dp_with_strategy(scale, RoundingStrategy::MidpointAwayFromZero);
    }
    if result.scale() < scale {
        result.rescale(scale);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pg_numeric_div_pads_scale_for_exact_result() {
        let result = pg_numeric_div(Decimal::from(675), Decimal::from(4));
        assert_eq!(result.scale(), 16);
        assert_eq!(result.to_string(), "168.7500000000000000");
    }

    #[test]
    fn test_pg_numeric_div_rounds_to_pg_scale_for_repeating_result() {
        let result = pg_numeric_div(Decimal::from(1000), Decimal::from(7));
        assert_eq!(result.scale(), 16);
        assert_eq!(result.to_string(), "142.8571428571428571");
    }
}
