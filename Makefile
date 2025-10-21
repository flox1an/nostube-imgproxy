.PHONY: build run test docker-build docker-run docker-compose-up docker-compose-down clean

# Build release binary
build:
	./build.sh --release

# Run locally
run:
	./build.sh run --release

# Quick check
check:
	export PATH="$$HOME/Library/Python/3.9/bin:$$HOME/.local/bin:$$PATH" && cargo check

# Build Docker image
docker-build:
	docker build -t rust-imgproxy:latest .

# Run Docker container
docker-run:
	docker run -p 8080:8080 -v $$(pwd)/cache:/cache rust-imgproxy:latest

# Start with docker-compose
docker-compose-up:
	docker-compose up -d

# Stop docker-compose
docker-compose-down:
	docker-compose down

# Clean build artifacts and cache
clean:
	cargo clean
	rm -rf cache/*

# Test with a sample image
test-image:
	@echo "Testing image resize..."
	curl -s "http://127.0.0.1:8080/insecure/f:webp/rs:fit:400:400/plain/https%3A%2F%2Fblossom.yakihonne.com%2F04e014f09abbc556cd58f586aa70b7528160383cb788a6d7428a5933be3ce894.jpeg" -o /tmp/test.webp
	@file /tmp/test.webp

# Test health endpoint
test-health:
	@curl http://127.0.0.1:8080/health
	@echo ""

# Show cache statistics
cache-stats:
	@echo "Original cache:"
	@find cache/original -type f 2>/dev/null | wc -l || echo "0"
	@echo "Processed cache:"
	@find cache/processed -type f 2>/dev/null | wc -l || echo "0"
	@echo "Total cache size:"
	@du -sh cache 2>/dev/null || echo "0"

