| column | issue | severity | affected_rows | fix |
|---|---|---|---|---|
| customer_id | 3 exact duplicate rows (CU-1011, CU-1034, CU-1058): 83 rows but 80 unique ids | high | 3 | Drop exact duplicates before any count |
| annual_spend_krw | Text "N/A" in a numeric column (CU-1013, CU-1059) | high | 2 | Turn it into an empty value and leave it out of sums |
| age | Impossible values 212 (CU-1024) and -1 (CU-1052), and 4 empty values | high | 6 | Set the impossible ages to empty and ask for the real ones |
| annual_spend_krw | 45,000,000 (CU-1065) is about 4 times the next highest value (11,110,000) | medium | 1 | Confirm with sales and keep it out of averages until checked |
| annual_spend_krw | Empty for CU-1006, CU-1037 and CU-1078 | medium | 3 | Fill in from billing, or mark as unknown |
| email | Empty in 5 rows and malformed in 3 (for example a missing domain after @) | medium | 8 | Validate the format and ask the customers for a correction |
| city | Same city spelled several ways: "Seoul" 37, " seoul" 9, "SEOUL" 3, "Busan" 15, "busan" 3 | medium | 15 | Trim spaces and use one spelling per city |
| signup_date | Three formats besides YYYY-MM-DD: MM/DD/YYYY 11, D MMM YYYY 9, YYYY.MM.DD 2 | medium | 22 | Decide MM/DD or DD/MM first (12/09/2025 is ambiguous), then write all as YYYY-MM-DD |
| segment | "vip" 8 and "regular " 5 differ in case or spacing, and 6 are empty | medium | 19 | Trim and lower-case, then derive the segment from annual_spend_krw |
| marketing_opt_in | Spellings Y, N, yes, no and TRUE are mixed, and 8 are empty | low | 24 | Map to Y or N and treat empty as no consent |
| phone | Three formats (010-1234-5678, 01012345678, +82 10 1234 5678) and 6 empty | low | 37 | Store one format, for example +82 without hyphens |
| name | Wrong case or leading spaces | low | 9 | Trim and use title case |
