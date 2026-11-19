# Samples

Sample data for trying Candy-Corn without bringing your own files. All of it is **synthetic**: every name, ID, amount and
comment is made up, and no real person or company is behind any row. Each file reaches the profiler through a different
input path (delimiter, text encoding, JSON flavor) and has a few planted problems, so the lint log, the outlier list and
the analysis tools of the LLM result (VIII) have something to find.

## Files

| Data file | Format | Rows | What is planted |
|---|---|---|---|
| `sales.csv` | comma, UTF-8 | 301 | Order 301 (Seoul) is 900000 against a median of 1434. The data of the 3-minute demo |
| `web_traffic.tsv` | tab, UTF-8 | 240 | A bot-like spike on `/pricing` on 2026-09-05 (48,210 sessions, bounce rate 0.97, 3 second visits, normal conversions) and a tracking outage on 2026-09-12 (conversions and revenue empty on every page) |
| `iot_sensors.ndjson` | one JSON object per line | 300 | Six sensors, a reading every 30 minutes. D-04 reads 92.4 °C once, D-05 vibration climbs to 7.8 mm/s, D-06 battery falls to 3.09 V, D-02 has no humidity value 7 times |
| `ecommerce_orders.json` | one pretty-printed JSON array | 200 | Order SO-104138 has quantity 480 (the median is 2), SO-104062 shipped in 21 days, 12 orders have a null discount, 19 were returned |
| `hr_employees_kr.csv` | comma, **CP949** (Korean Excel), Korean headers | 150 | One executive (K2024002) earns 25,000 (10,000 KRW units, written with a thousands comma) against 9,470 for everyone else; 8 employees hired in 2026 have no performance grade |
| `support_tickets.csv` | semicolon, **UTF-8 with BOM**, CRLF | 220 | Ticket T-50073 waited 2,880 minutes for a first reply, T-50009 stayed open 312 hours, 57 tickets have no CSAT; subjects contain commas, quotes and a semicolon |
| `bank_transactions.txt` | pipe, UTF-8 | 247 | Three 1.00 card checks at 02:41 to 02:44 on 2026-10-17, then a 9,850.00 wire to an unknown beneficiary at 03:12. Amounts and balances use thousands commas |
| `energy_meter_utf16.txt` | tab, **UTF-16 LE with BOM** (Excel "Unicode Text") | 180 | HQ Tower used 24,900 kWh on 2026-09-22 (normal 4,859) with a 2,450 kW peak, while weather and occupancy were normal |
| `survey_responses.csv` | comma, UTF-8 | 120 | One answer took 186 minutes, 22 have no support rating, 40 have no comment; comments contain commas, quotes, line breaks and Korean text |
| `customers_messy.csv` | comma, UTF-8, CRLF | 83 | A deliberately dirty CRM export: 3 exact duplicate rows, ages 212 and -1, "N/A" in a numeric column, a 45,000,000 spend, mixed upper and lower case, stray spaces, three date formats, mixed phone formats, malformed emails |
| `flow.canvas.json` | graph file | | A graph for the graph tool (`--canvas`) with two test cases |

## Try them

In the studio, open the **📁 Samples** menu in the left pane and pick a `📋` blueprint. It loads the data, the role, the
constraints, the output format and the target model. Press Ctrl+Enter to compile.

From a terminal:

```
Candy-Corn.exe --cli --blueprint samples\web_traffic.kiro
Candy-Corn.exe --cli --target gemini --data samples\bank_transactions.txt
```

### Sample replies (no API key needed)

`replies\` holds one reply per blueprint, written the way a good model would answer: it cites the planted problems with
the numbers of the data. Compile a blueprint, then press **📂 Open reply…** in VIII and choose the reply with the same
name (`web_traffic.kiro` → `replies\web_traffic.reply.json`). Candy-Corn checks it against the output format like a live
reply and opens the **📈 Analysis** tab: overview tiles, which columns and outliers of the data the reply mentions, a
table you can sort and filter, column statistics and a bar or donut chart.

| Blueprint | Target | Output | Reply |
|---|---|---|---|
| `sales_outliers.kiro` | Claude | JSON | `replies\sales_outliers.reply.json` |
| `web_traffic.kiro` | Claude | JSON | `replies\web_traffic.reply.json` |
| `iot_sensors.kiro` | Gemini | JSON | `replies\iot_sensors.reply.json` |
| `ecommerce_orders.kiro` | GPT | table | `replies\ecommerce_orders.reply.md` |
| `hr_employees_kr.kiro` | Claude | JSON (Korean) | `replies\hr_employees_kr.reply.json` |
| `support_tickets.kiro` | GPT | table | `replies\support_tickets.reply.md` |
| `bank_transactions.kiro` | Claude | JSON | `replies\bank_transactions.reply.json` |
| `energy_meter.kiro` | Gemini | table | `replies\energy_meter.reply.md` |
| `survey_responses.kiro` | Claude | JSON | `replies\survey_responses.reply.json` |
| `customers_messy.kiro` | GPT | table | `replies\customers_messy.reply.md` |

To run a blueprint against a real model, add a key in **🔑 API keys** and press **▶ Run live**.

## Regenerating

`tools\gen-samples.ps1` writes the nine generated data files again, byte for byte, because it uses a fixed random seed
(Windows PowerShell 5.1 or later):

```
powershell -ExecutionPolicy Bypass -File tools\gen-samples.ps1
```

`sales.csv`, `flow.canvas.json`, the `.kiro` blueprints and `replies\` are written by hand and are not touched. The
tests in `tests\samples.rs` read these files with the real executable and check the planted problems, the encodings and
that every blueprint and reply still fits the data.