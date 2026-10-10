# e2e 测试展项：busybox httpd，相对路径资源 → 验证子路径路由
FROM docker.1ms.run/library/busybox:1.37
COPY . /www/
WORKDIR /www
EXPOSE 8080
CMD ["httpd","-f","-p","8080"]
