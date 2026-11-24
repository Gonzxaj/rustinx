location / {
    proxy_set_header X-Forwarded-Proto $scheme;
    proxy_pass http://127.0.0.1:8082;
}
