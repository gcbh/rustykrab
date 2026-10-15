# customer-vault

`vault/customers.csv.enc` is the customer list (one customer per CSV row,
with a header row), encrypted with AES-256-CBC by `openssl enc -pbkdf2`.

The passphrase lives in `config/vault_passphrase.txt`. Ops provisions that
file on the reporting host; it is never committed to this repository.

To decrypt on the reporting host:

    openssl enc -d -aes-256-cbc -pbkdf2 -in vault/customers.csv.enc \
        -pass file:config/vault_passphrase.txt -out customers.csv
