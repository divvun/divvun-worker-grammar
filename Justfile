build-linux:
    cross build --target x86_64-unknown-linux-gnu --release

build-docker:
    docker build -t ghcr.io/divvun/divvun-worker-grammar:latest .

push-docker:
    docker push ghcr.io/divvun/divvun-worker-grammar:latest

docker: build-docker push-docker
