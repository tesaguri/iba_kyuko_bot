extern crate tendril;

use errors::*;
use kuchiki::parse_html;
use kuchiki::traits::*;
use self::tendril::Tendril;
use self::tendril::fmt::UTF8;
use self::util::*;
use std::collections::HashSet;
use ::Kyuko;

pub fn scrape<T: Into<Tendril<UTF8>>>(html: T) -> Result<(String, HashSet<Kyuko>)> {
    // <!-- Example DOM tree (extract) -->
    //
    // <div id="tabbox">
    //   <div class="gakubulist">
    //     <a href="index-student2.php?g=3" class="tab selected">教養教育<br>&nbsp;<br>60件</a>&nbsp;&nbsp;　
    //     <a href="index-student2.php?g=4" class="tab">人文学部<br>（研究科）<br>1件</a>
    //     <a href="index-student2.php?g=5" class="tab">教育学部<br>（研究科）<br>1件</a>
    //     <a href="index-student2.php?g=6" class="tab">理学部<br>（研究科）<br>1件</a>
    //     <a href="index-student2.php?g=7" class="tab">工学部<br>（研究科）<br>15件</a>
    //     <a href="index-student2.php?g=8" class="tab">農学部<br>（研究科）<br>10件</a>
    //   </div>
    //   <div id="eventlist">
    //     <table class="citem">
    //       <tbody>
    //         <tr>
    //           <td rowspan="2"><img src="img/hokou.png" alt="【補講】" width="60px"></td>
    //           <td class="date">2016年04月06日(水)　1.2講時</td> <!-- May contain malformed date -->
    //           <!-- The format of periods is not unified. Other examples are: "1,2講時", "1-3講時", "3・4講時". -->
    //           <td class="kamoku">科目 [教員]</td>
    //         </tr>
    //         <tr>
    //           <td class="memo" colspan="2">備考</td>
    //         </tr>
    //       </tbody>
    //     </table>
    //     <!-- ... -->
    //   </div>
    // </div>

    let document = parse_html().one(html);

    let dept = document
        .select("#tabbox > .gakubulist > a.selected")
        .expect("failed to parse the selector")
        .next().ok_or_else::<Error,_>(|| "unable to determine the department".into())?
        .as_node().children().next().ok_or_else::<Error,_>(|| "expected text node".into())?;
    let dept = dept.as_text().ok_or_else::<Error,_>(|| "expected text node".into())?.borrow().clone();

    let mut kyukos = HashSet::new();

    for tbody in document.select("#eventlist > table.citem > tbody").expect("failed to parse the selector") {
        match parse_kyuko_tbody(tbody.as_node()) {
            Ok(k) => { kyukos.insert(k); },
            Err(e) => {
                error!("error: {}", e.description());
                for e in e.iter().skip(1) {
                    error!("caused by: {}", e);
                }
            },
        }
    }

    Ok((dept, kyukos))
}

mod util {
    use chrono::NaiveDate;
    use errors::*;
    use kuchiki::NodeRef;
    use ::{Kyuko, Periods};

    pub fn parse_kyuko_tbody(tbody: &NodeRef) -> Result<Kyuko> {
        macro_rules! msg {
            ($s:expr) => (|| <String as Into<Error>>::into(format!("{}; DOM tree: {}", $s, tbody.to_string())));
            () => (msg!("failed to parse class information"));
        }

        let mut trs = tbody.children().filter(|node| node.as_element().is_some());

        let mut tds = trs.next().ok_or_else(msg!("expected <tr>"))?.children()
            .filter(|node| node.as_element().is_some()); // TODO: replace /w mem::discriminant once it's stabilized.

        let kind = tds.next().ok_or_else(msg!("expected <td> for information kind"))?
            .first_child().ok_or_else(msg!("expected element for information kind"))?;
        let mut kind = kind.as_element().ok_or_else(msg!("expected element for information kind"))?
            .attributes.borrow()
            .get("alt").ok_or_else(msg!("expected alt attribute for information kind"))?.to_string();
        if kind.ends_with('】') {
            kind.pop();
        }
        if kind.starts_with('【') {
            kind.remove(0);
        }

        let date = tds.next().ok_or_else(msg!("expected <td> for date"))?
            .first_child().ok_or_else(msg!("expected text node for date"))?;
        let date = date.as_text().ok_or_else(msg!("expected text node for date"))?.borrow();
        let (date, periods) = parse_date(&date).chain_err(msg!())?;

        let title = tds.next().ok_or_else(msg!("expected <td>"))?
            .first_child().ok_or_else(msg!("expected text node for class title"))?;
        let title = title.as_text().ok_or_else(msg!("expected text node for class title"))?.borrow();
        let (title, lecturer) = parse_kamoku(&title)?;

        let remarks = trs.next().ok_or_else(msg!("expected <tr> for remarks"))?
            .first_child().ok_or_else(msg!("expected <td> for remarks"))?
            .first_child().and_then(|text| {
                text.as_text().map(|t| t.borrow().clone())
            });

        Ok(Kyuko {
            kind: kind,
            date: date,
            periods: periods,
            title: title,
            lecturer: lecturer,
            remarks: remarks,
        })
    }

    pub fn parse_date(src: &str) -> Result<(NaiveDate, Periods)> {
        let date = src.split('(')
            .next().ok_or_else::<Error,_>(|| format!("unable to find date string in: {}", src).into())?;
        let date = parse_date_str(date)?;

        let periods = src.split_whitespace().nth(1)
            .ok_or_else::<Error,_>(|| format!("unable to find periods string in: {}", src).into())?;
        let periods = parse_periods(periods);

        Ok((date, periods))
    }

    pub fn parse_kamoku(src: &str) -> Result<(String, String)> {
        let i = src.rfind('[').ok_or_else::<Error,_>(|| "unable to find lecturer name".into())?;
        let (title, lecturer) = src.split_at(i);
        let title = title.trim_right();
        let lecturer = lecturer.trim_left_matches('[').trim_right_matches(']');
        Ok((title.to_string(), lecturer.to_string()))
    }

    pub fn parse_date_str(src: &str) -> Result<NaiveDate> {
        NaiveDate::parse_from_str(src, "%Y年%m月%d日").chain_err(|| "unable to parse the date")
    }

    pub fn parse_periods(src: &str) -> Periods {
        fn isdigit(c: u8) -> bool { b'0' <= c && c <= b'9' }
        fn atoi(c: u8) -> u8 { c - b'0' }

        let mut ret = Vec::new();

        macro_rules! push {
            ($c:expr) => {{
                if isdigit($c) {
                    ret.push(atoi($c));
                }
            }}
        }

        for a in src.as_bytes().split(|&c| (c < b'0' || b'9' < c) && c != b'-') {
            for w in a.windows(3) {
                match w {
                    &[c, b'-', d] if isdigit(c) && isdigit(d) => ret.extend(atoi(c)..(atoi(d)+1)),
                    &[_, _, c] => push!(c),
                    _ => unreachable!(),
                }
            }

            for &c in a.get(0) { push!(c); }
            for &c in a.get(1) { push!(c); }
        }

        Periods::from(ret)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
	    use chrono::NaiveDate;
        use ::Periods;

        #[test]
        fn parse_date_test() {
            assert_eq!(
                parse_date("2016年12月19日(月)　1講時").unwrap(),
                (NaiveDate::from_ymd(2016, 12, 19), Periods::from(vec![1u8]))
            );
        }

        #[test]
        fn parse_periods_test() {
            macro_rules! test_eq {
                ($src:expr, $expect:expr) => {{
                    assert_eq!(parse_periods($src).as_ref(), &$expect);
                }};
            }

            test_eq!("1講時", [1]);
            test_eq!("3.4講時", [3, 4]);
            test_eq!("5,6講時", [5, 6]);
            test_eq!("1-2講時", [1, 2]);
            test_eq!("3・4講時", [3, 4]);
            test_eq!("3-5講時", [3, 4, 5]);
            test_eq!("1-3,5講時", [1, 2, 3, 5]);
            test_eq!("1,3-5,6講時", [1, 3, 4, 5, 6]);
        }
    }
}

