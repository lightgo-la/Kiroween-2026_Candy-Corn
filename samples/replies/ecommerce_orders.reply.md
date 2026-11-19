| order_id | problem | orders_affected | suggested_action |
|---|---|---|---|
| SO-104138 | Quantity 480 of Books with a 20% discount (the median order has quantity 2) | 1 | Confirm that it is a bulk B2B order and that the discount was approved |
| SO-104062 | Shipping took 21 days (the median is 2 days, the 95th percentile 5) | 1 | Contact the carrier and the customer, and check for a lost parcel |
| SO-104018, SO-104036, SO-104054, SO-104059, SO-104060, SO-104093, SO-104106, SO-104113, SO-104116, SO-104143, SO-104154, SO-104192 | discount_pct is null | 12 | Record 0 where no discount applied, or find out why the field was not filled |
| (all orders) | 19 of 200 orders were returned (9.5%); the data has no return reason | 19 | Break the returns down by category once a reason is recorded |
