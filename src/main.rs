use anyhow::{Context, Result};
use chrono::NaiveDate;
use dashmap::DashMap;
use dotenv::dotenv;
use fantoccini::{elements::Element, Client, ClientBuilder, Locator};
use regex::Regex;
use std::{
    env,
    process::{Child, Command},
    sync::Arc,
};
use tokio::time::{timeout, Duration};

#[derive(Debug)]
struct KepcoDate {
    claim_date: Option<NaiveDate>,
    start_date: Option<NaiveDate>,
    end_date: Option<NaiveDate>,
    usage: f64,
    amount: i64,
    payment: i64,
    unpaid: i64,
    payment_method: Option<String>,
    payment_date: Option<NaiveDate>,
}

// 요소 대기
async fn wait_for_element(
    client: &Client,
    locator: Locator<'_>,
    chromedriver_process: &mut Child,
) -> Result<Option<Element>> {
    match client.wait().for_element(locator).await {
        Ok(element) => Ok(Some(element)),
        Err(e) => {
            eprintln!("Failed to find the element: {:?}\n {}", locator, e);
            client
                .clone()
                .close()
                .await
                .context("Failed to close client")?;
            chromedriver_process
                .kill()
                .expect("failed to kill ChromeDriver");
            Err(anyhow::anyhow!("Failed to find the element: {:?}", e))
        }
    }
}

// 요소 클릭
async fn click_element(client: &Client, locator: Locator<'_>) -> Result<()> {
    if let Ok(element) = client.find(locator).await {
        element
            .click()
            .await
            .context(format!("Failed to click the element: {:?}", locator))?;
        println!("Element clicked successfully: {:?}", locator);
    } else {
        eprintln!("Failed to find the element: {:?}", locator);
        return Err(anyhow::anyhow!("Failed to find the element: {:?}", locator));
    }
    Ok(())
}

// 요소에 값 입력
async fn enter_value_in_element(client: &Client, locator: Locator<'_>, text: &str) -> Result<()> {
    if let Ok(element) = client.find(locator).await {
        if let Err(e) = element.send_keys(text).await {
            eprintln!("Failed to enter text: {}", e);
        } else {
            println!("Text entered successfully: {:?}", locator);
        }
    } else {
        eprintln!("Failed to find the input element: {:?}", locator);
    }
    Ok(())
}

// 요소 비활성화 대기
async fn wait_for_element_hidden(
    client: &Client,
    locator: Locator<'_>,
    chromedriver_process: &mut Child,
    duration: Duration,
) -> Result<()> {
    let element = match wait_for_element(client, locator, chromedriver_process).await? {
        Some(element) => element,
        None => return Err(anyhow::anyhow!("Failed to find the element: {:?}", locator)),
    };

    let element_hidden = timeout(duration, async {
        loop {
            match element.attr("aria-hidden").await {
                Ok(Some(value)) if value == "true" => {
                    println!("Element is hidden (aria-hidden=\"true\")");
                    break;
                }
                Ok(_) => {
                    eprintln!("Element is not hidden, retrying...");
                }
                Err(e) => {
                    eprintln!("Failed to get aria-hidden attribute: {}", e);
                }
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    })
    .await;

    if element_hidden.is_err() {
        Err(anyhow::anyhow!(
            "Failed to find the element within the given duration"
        ))
    } else {
        Ok(())
    }
}

// 요소 클릭 반복
async fn click_element_with_retries(
    client: &Client,
    locator: Locator<'_>,
    max_attempts: u32,
) -> Result<()> {
    let mut attempts = 0;
    loop {
        if attempts >= max_attempts {
            return Err(anyhow::anyhow!(
                "Failed to click the element after {} attempts",
                max_attempts
            ));
        }
        match client.find(locator).await {
            Ok(element) => match element.click().await {
                Ok(_) => {
                    println!(
                        "Element clicked successfully after {} attempts",
                        attempts + 1
                    );
                    return Ok(());
                }
                Err(e) => {
                    eprintln!(
                        "Failed to click the element (attempt {}): {}",
                        attempts + 1,
                        e
                    );
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    attempts += 1;
                }
            },
            Err(e) => {
                eprintln!(
                    "Retrying to find the element (attempt {}): {}",
                    attempts + 1,
                    e
                );
                // 요소를 찾지 못하면 잠시 대기 후 다시 시도
                tokio::time::sleep(Duration::from_secs(1)).await;
                attempts += 1;
            }
        }
    }
}

// 자식 요소들의 ID -> DashMap
async fn get_children_ids_to_map(
    client: &Client,
    parent_id: &str,
) -> Result<Arc<DashMap<String, ()>>> {
    let script = format!(
        r#"
        let children = document.getElementById('{}').children;
        let ids = [];
        for (let i = 0; i < children.length; i++) {{
            ids.push(children[i].id);
        }}
        return ids;
        "#,
        parent_id
    );

    let result = client
        .execute(&script, vec![])
        .await
        .context("Failed to execute script to get children IDs")?;

    let ids: Vec<String> = result
        .as_array()
        .context("Expected an array from the script result")?
        .iter()
        .map(|v| {
            v.as_str()
                .context("Expected a string in the array")
                .map(|s| s.to_string())
        })
        .collect::<Result<Vec<String>>>()?;

    let map = Arc::new(DashMap::new());
    for id in ids {
        map.insert(id, ());
    }

    Ok(map)
}

// parsing 청구 기간
fn parse_date(date_str: &str) -> Result<NaiveDate> {
    let date_with_day = if date_str.len() == 7 {
        format!("{}.01", date_str) // 일자를 1로 설정
    } else {
        date_str.to_string()
    };
    NaiveDate::parse_from_str(&date_with_day, "%Y.%m.%d").context("Failed to parse date")
}

// parsing 대상 기간
fn parse_date_range(date_range: &str) -> Result<(Option<NaiveDate>, Option<NaiveDate>)> {
    let dates: Vec<&str> = date_range.split('-').collect();
    let start_date = parse_date(dates[0]).ok();
    let end_date = parse_date(dates[1]).ok();
    Ok((start_date, end_date))
}

// parsing 사용량
fn parse_use_kwh(kwh_str: &str) -> Result<f64> {
    let cleaned_str = kwh_str.replace(",", "").replace("kWh", "");
    cleaned_str
        .parse::<f64>()
        .context("Failed to parse use kWh")
}

// parsing 요금
fn parse_amount(amount_str: &str) -> Result<i64> {
    let amount = amount_str
        .split('(')
        .next()
        .unwrap_or(amount_str)
        .replace("원", "")
        .replace(",", "");
    amount.parse::<i64>().context("Failed to parse amount")
}

// parsing 지불방법, 기간
fn parse_payment_method(payment_str: &str) -> Result<(Option<String>, Option<NaiveDate>)> {
    let parts: Vec<&str> = payment_str.split('/').collect();
    let method = if !parts[0].is_empty() {
        Some(parts[0].to_string())
    } else {
        None
    };
    let date = if parts.len() > 1 {
        Some(parse_date(parts[1])?)
    } else {
        None
    };
    Ok((method, date))
}

// get_and_parsing_data
async fn extract_data(client: &Client, parent_id: &str) -> Result<KepcoDate> {
    async fn get_text_by_xpath(client: &Client, xpath: &str) -> Option<String> {
        match client.find(Locator::XPath(xpath)).await.ok() {
            Some(element) => element.text().await.ok(),
            None => None,
        }
    }

    let claim_date_row = get_text_by_xpath(
        client,
        &format!(
            "//*[@id='{}']//span[contains(@id, '_txt_payYm')]",
            parent_id
        ),
    )
    .await;
    let date_range = get_text_by_xpath(
        client,
        &format!(
            "//*[@id='{}']//span[contains(@id, '_txt_gigan')]",
            parent_id
        ),
    )
    .await;
    let usage_row = get_text_by_xpath(
        client,
        &format!(
            "//*[@id='{}']//span[contains(@id, '_txt_useKwh')]",
            parent_id
        ),
    )
    .await;
    let amount_row = get_text_by_xpath(
        client,
        &format!("//*[@id='{}']//span[contains(@id, '_txt_pay')]", parent_id),
    )
    .await;
    let payment_row = get_text_by_xpath(
        client,
        &format!(
            "//*[@id='{}']//span[contains(@id, '_txt_payAmt')]",
            parent_id
        ),
    )
    .await;
    let unpaid_row = get_text_by_xpath(
        client,
        &format!(
            "//*[@id='{}']//span[contains(@id, '_txt_payAmt')]",
            parent_id
        ),
    )
    .await;
    let payment_option = get_text_by_xpath(
        client,
        &format!(
            "//*[@id='{}']//span[contains(@id, '_txt_payGubnNDay')]",
            parent_id
        ),
    )
    .await;

    let claim_date = claim_date_row.map(|date| parse_date(&date)).transpose()?;
    let (start_date, end_date) = date_range
        .map(|range| parse_date_range(&range))
        .transpose()?
        .unwrap_or((None, None));
    let usage = usage_row.map_or(Ok(0.0), |kwh| parse_use_kwh(&kwh))?;
    let amount = amount_row.map_or(Ok(0), |amount| parse_amount(&amount))?;
    let payment = payment_row.map_or(Ok(0), |amount| parse_amount(&amount))?;
    let unpaid = unpaid_row.map_or(Ok(0), |amount| parse_amount(&amount))?;
    let (payment_method, payment_date) =
        payment_option.map_or(Ok((None, None)), |s| parse_payment_method(&s))?;

    Ok(KepcoDate {
        claim_date,
        start_date,
        end_date,
        usage,
        amount,
        payment,
        unpaid,
        payment_method,
        payment_date,
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();

    let url = "http://localhost:4444";
    let user_id = env::var("USER_ID").expect("");
    let user_pw = env::var("USER_PW").expect("");
    let user_number = env::var("USER_NUMBER").expect("");

    // ChromeDriver 실행 경로 (Homebrew로 설치된 경로)
    let chromedriver_path = "/opt/homebrew/bin/chromedriver"; // 또는 설치된 ChromeDriver의 경로

    // ChromeDriver 실행
    let mut chromedriver_process = Command::new(chromedriver_path)
        .arg("--port=4444") // 포트를 4444로 설정
        .spawn()
        .expect("failed to start ChromeDriver");

    // ChromeDriver 완전 시작 대기
    tokio::time::sleep(Duration::from_secs(2)).await;

    // WebDriver 서버에 연결
    let client = loop {
        match ClientBuilder::native().connect(url).await {
            Ok(client) => break client,
            Err(e) => {
                eprintln!("Retrying to connect to WebDriver: {}", e);
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    };

    // 브라우저 창 크기 설정
    client.set_window_rect(0, 0, 774, 857).await?;
    // 페이지 이동
    client
        .goto("https://online.kepco.co.kr")
        .await
        .context("Failed to navigate")?;

    // menu button 로드 대기
    wait_for_element(
        &client,
        Locator::Id("mf_wfm_header_gnb_btnSiteMap"),
        &mut chromedriver_process,
    )
    .await?;
    // menu button 클릭
    click_element(&client, Locator::Id("mf_wfm_header_gnb_btnSiteMap")).await?;

    // login form 로드 대기
    wait_for_element(
        &client,
        Locator::Id("mf_wfm_header_gnb_mobileGoLogin"),
        &mut chromedriver_process,
    )
    .await?;
    // login form 클릭
    click_element(&client, Locator::Id("mf_wfm_header_gnb_mobileGoLogin")).await?;

    // id 입력 로드 대기
    wait_for_element(
        &client,
        Locator::Id("mf_wfm_header_gnb_login_popup_wframe_ui_id"),
        &mut chromedriver_process,
    )
    .await?;
    // id 입력
    enter_value_in_element(
        &client,
        Locator::Id("mf_wfm_header_gnb_login_popup_wframe_ui_id"),
        &user_id,
    )
    .await?;
    // pw 입력
    enter_value_in_element(
        &client,
        Locator::Id("mf_wfm_header_gnb_login_popup_wframe_ui_pw"),
        &user_pw,
    )
    .await?;
    // 로그인 버튼 클릭
    click_element(
        &client,
        Locator::Id("mf_wfm_header_gnb_login_popup_wframe_btn_login"),
    )
    .await?;

    // /html/body/div[2]/div[3]/div/div/div[4]/div/div[2]/div[1]/a[3]
    // 요금 조회 버튼 클릭 반복 시도
    click_element_with_retries(
        &client,
        Locator::XPath("/html/body/div[2]/div[3]/div/div/div[4]/div/div[2]/div[1]/a[3]"),
        10,
    )
    .await?;

    // 필드 로드 대기
    wait_for_element(
        &client,
        Locator::Id("mf_wfm_layout_inp_searchCustNo"),
        &mut chromedriver_process,
    )
    .await?;

    // 1초 동안 대기(로딩...)
    //println!("Waiting for 1 SEC...");
    //tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    // 로딩 대기
    wait_for_element_hidden(
        &client,
        Locator::Id("mf_wq_uuid_1_wq_processMsgComp"),
        &mut chromedriver_process,
        Duration::from_secs(20),
    )
    .await?;

    // 사용자 번호 입력
    enter_value_in_element(
        &client,
        Locator::Id("mf_wfm_layout_inp_searchCustNo"),
        &user_number,
    )
    .await?;
    // 사용자 번호 입력 후 검색 버튼 클릭
    click_element(&client, Locator::Id("mf_wfm_layout_btn_search")).await?;

    // 상세 요금 버튼 로드 대기
    wait_for_element(
        &client,
        Locator::Id("mf_wfm_layout_ui_generator_0_btn_moveDetail"),
        &mut chromedriver_process,
    )
    .await?;
    // 창에서 스크롤을 강제로 맨 아래로 내리기
    client
        .execute("window.scrollTo(0, document.body.scrollHeight);", vec![])
        .await?;
    // 상세 요금 버튼 클릭
    click_element(
        &client,
        Locator::Id("mf_wfm_layout_ui_generator_0_btn_moveDetail"),
    )
    .await?;

    // '1년' 옵션을 선택
    click_element_with_retries(&client, Locator::XPath("//option[text()='1년']"), 10).await?;

    // 로딩 대기
    wait_for_element_hidden(
        &client,
        Locator::Id("mf_wq_uuid_1_wq_processMsgComp"),
        &mut chromedriver_process,
        Duration::from_secs(20),
    )
    .await?;

    // 자식 요소들의 ID를 가져와서 DashMap에 저장
    let map = get_children_ids_to_map(&client, "mf_wfm_layout_ui_generator").await?;

    // DashMap의 내용을 출력
    for id in map.iter() {
        println!("ID: {}", id.key());
    }






    // 2분 동안 대기
    println!("Waiting for 2 minutes...");
    tokio::time::sleep(tokio::time::Duration::from_secs(120)).await;

    // 브라우저 닫기
    client.close().await.context("Failed to close the client")?;
    // ChromeDriver 프로세스 종료
    chromedriver_process
        .kill()
        .expect("failed to kill ChromeDriver");

    Ok(())
}
