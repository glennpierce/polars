use super::*;

#[test]
#[cfg(feature = "unique_counts")]
fn test_list_broadcast() {
    // simply test if this runs
    df![
        "g" => [1, 1, 1],
        "a" => [1, 2, 3],
    ]
    .unwrap()
    .lazy()
    .groupby([col("g")])
    .agg([col("a").unique_counts() * count()])
    .collect()
    .unwrap();
}

#[test]
fn ternary_expand_sizes() -> Result<()> {
    let df = df! {
        "a" => [Some("a1"), None, None],
        "b" => [Some("b1"), Some("b2"), None]
    }?;
    let out = df
        .lazy()
        .with_column(
            when(not(lit(true)))
                .then(lit("unexpected"))
                .when(not(col("a").is_null()))
                .then(col("a"))
                .when(not(col("b").is_null()))
                .then(col("b"))
                .otherwise(lit("otherwise"))
                .alias("c"),
        )
        .collect()?;
    let vals = out
        .column("c")?
        .utf8()?
        .into_no_null_iter()
        .collect::<Vec<_>>();
    assert_eq!(vals, &["a1", "b2", "otherwise"]);
    Ok(())
}

#[test]
#[cfg(feature = "strings")]
fn includes_null_predicate_3038() -> Result<()> {
    let df = df! {
        "a" => [Some("a1"), None, None],
    }?;
    let res = df
        .lazy()
        .with_column(
            when(col("a").map(
                move |s| {
                    s.utf8()?
                        .to_lowercase()
                        .contains("not_exist")
                        .map(Into::into)
                },
                GetOutput::from_type(DataType::Boolean),
            ))
            .then(lit("unexpected"))
            .when(col("a").eq(lit("a1".to_string())))
            .then(lit("good hit"))
            .otherwise(Expr::Literal(LiteralValue::Null))
            .alias("b"),
        )
        .collect()?;

    let exp_df = df! {
        "a" => [Some("a1"), None, None],
        "b" => [Some("good hit"), None, None],
    }?;
    assert!(res.frame_equal_missing(&exp_df));

    let df = df! {
        "a" => ["a1", "a2", "a3", "a4", "a2"],
        "b" => [Some("tree"), None, None, None, None],
    }?;
    let res = df
        .lazy()
        .with_column(
            when(col("b").map(
                move |s| {
                    s.utf8()?
                        .to_lowercase()
                        .contains("non-existent")
                        .map(Into::into)
                },
                GetOutput::from_type(DataType::Boolean),
            ))
            .then(lit("weird-1"))
            .when(col("a").eq(lit("a1".to_string())))
            .then(lit("ok1"))
            .when(col("a").eq(lit("a2".to_string())))
            .then(lit("ok2"))
            .when(lit(true))
            .then(lit("ft"))
            .otherwise(Expr::Literal(LiteralValue::Null))
            .alias("c"),
        )
        .collect()?;
    let exp_df = df! {
        "a" => ["a1", "a2", "a3", "a4", "a2"],
        "b" => [Some("tree"), None, None, None, None],
        "c" => ["ok1", "ok2", "ft", "ft", "ok2"]
    }?;
    assert!(res.frame_equal_missing(&exp_df));

    Ok(())
}

#[test]
fn test_when_then_otherwise_cats() -> Result<()> {
    let lf = df!["book" => [Some("bookA"),
        None,
        Some("bookB"),
        None,
        Some("bookA"),
        Some("bookC"),
        Some("bookC"),
        Some("bookC")],
        "user" => [Some("bob"), Some("bob"), Some("bob"), Some("tim"), Some("lucy"), Some("lucy"), None, None]
    ]?.lazy();

    let out = lf
        .with_column(col("book").cast(DataType::Categorical(None)))
        .with_column(col("user").cast(DataType::Categorical(None)))
        .with_column(
            when(col("book").eq(Null {}.lit()))
                .then(col("user"))
                .otherwise(col("book"))
                .alias("a"),
        )
        .collect()?;

    assert_eq!(
        out.column("a")?
            .categorical()?
            .iter_str()
            .flatten()
            .collect::<Vec<_>>(),
        &["bookA", "bob", "bookB", "tim", "bookA", "bookC", "bookC", "bookC"]
    );

    Ok(())
}

#[test]
fn test_when_then_otherwise_single_bool() -> Result<()> {
    let df = df![
        "key" => ["a", "b", "b"],
        "val" => [Some(1), Some(2), None]
    ]?;

    let out = df
        .lazy()
        .groupby_stable([col("key")])
        .agg([when(col("val").null_count().gt(lit(0)))
            .then(Null {}.lit())
            .otherwise(col("val").sum())
            .alias("sum_null_prop")])
        .collect()?;

    let expected = df![
        "key" => ["a", "b"],
        "sum_null_prop" => [Some(1), None]
    ]?;

    assert!(out.frame_equal_missing(&expected));

    Ok(())
}

#[test]
#[cfg(feature = "unique_counts")]
fn test_update_groups_in_cast() -> Result<()> {
    let df = df![
        "group" =>  ["A" ,"A", "A", "B", "B", "B", "B"],
        "id"=> [1, 2, 1, 4, 5, 4, 6],
    ]?;

    // optimized to
    // col("id").unique_counts().cast(int64) * -1
    // in aggregation that cast coerces a list and the cast may forget to update groups
    let out = df
        .lazy()
        .groupby_stable([col("group")])
        .agg([col("id").unique_counts() * lit(-1)])
        .collect()?;

    let expected = df![
        "group" =>  ["A" ,"B"],
        "id"=> [AnyValue::List(Series::new("", [-2i64, -1])), AnyValue::List(Series::new("", [-2i64, -1, -1]))]
    ]?;

    assert!(out.frame_equal(&expected));
    Ok(())
}

#[test]
fn test_when_then_otherwise_sum_in_agg() -> Result<()> {
    let df = df![
        "groups" => [1, 1, 2, 2],
        "dist_a" => [0.1, 0.2, 0.5, 0.5],
        "dist_b" => [0.8, 0.2, 0.5, 0.2],
    ]?;

    let q = df
        .lazy()
        .groupby([col("groups")])
        .agg([when(all().exclude(["groups"]).sum().eq(lit(1)))
            .then(all().exclude(["groups"]).sum())
            .otherwise(lit(NULL))])
        .sort("groups", Default::default());

    let expected = df![
        "groups" => [1, 2],
        "dist_a" => [None, Some(1.0f64)],
        "dist_b" => [Some(1.0f64), None]
    ]?;
    assert!(q.collect()?.frame_equal_missing(&expected));

    Ok(())
}
