FROM pypy:2-5.9.0

RUN mkdir -p /app
ADD . /app

WORKDIR /app
ENV PATH=$PATH:/root/.cargo/bin

RUN \
    apt-get update && \
    apt-get install -y -qq libexpat1-dev gcc libssl-dev libffi-dev && \
    curl https://sh.rustup.rs | sh -s -- -y && \
    make clean && \
    pip install -r requirements.txt && \
    pypy setup.py develop

CMD ["autopush"]
