FROM alpine:latest

RUN apk add --no-cache curl bash coreutils

COPY run.sh /run.sh

RUN chmod +x /run.sh

CMD ["/run.sh"]
