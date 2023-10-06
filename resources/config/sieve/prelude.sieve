# Convert body to plain text
let "text_body" "body.to_text";

# Obtain all URLs in the body
let "body_urls" "tokenize(text_body, 'uri')";

# Obtain all URLs in href and src attributes
let "html_body_urls" "html_attrs(body.html, '', ['href', 'src'])";

# Obtain all URLs in the subject, combine them with all other URLs and remove duplicates
let "urls" "dedup(tokenize(header.subject, 'uri') + body_urls + html_body_urls)";

# Obtain thread name and subject
let "subject_lc" "to_lowercase(header.subject)";
let "thread_name" "thread_name(header.subject)";

# Obtain all recipients
let "recipients" "to_lowercase(header.to:cc:bcc[*].addr[*])";
let "recipients_clean" "winnow(dedup(recipients))";
let "recipients_to" "header.to[*].addr[*]";
let "recipients_cc" "header.cc[*].addr[*]";

# Obtain From parts
let "from_name" "to_lowercase(trim(header.from.name))";
let "from_addr" "to_lowercase(trim(header.from.addr))";
let "from_local" "email_part(from_addr, 'local')";
let "from_domain" "email_part(from_addr, 'domain')";
let "from_domain_sld" "domain_part(from_domain, 'sld')";

# Obtain Envelope From parts
let "envfrom_local" "email_part(envelope.from, 'local')";
let "envfrom_domain" "email_part(envelope.from, 'domain')";
let "envfrom_domain_sld" "domain_part(envfrom_domain, 'sld')";

# Obtain HELO domain SLD
let "helo_domain_sld" "domain_part(env.helo_domain, 'sld')";
