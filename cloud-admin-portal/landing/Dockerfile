FROM nginx:alpine
COPY nginx.conf /etc/nginx/conf.d/default.conf
COPY index.html /usr/share/nginx/html/index.html
COPY install.sh /usr/share/nginx/html/install.sh
COPY install-sh9.sh /usr/share/nginx/html/install-sh9.sh
COPY skill.md /usr/share/nginx/html/skill.md
COPY sdk.html /usr/share/nginx/html/sdk.html
COPY docs.html /usr/share/nginx/html/docs.html
COPY releases/ /usr/share/nginx/html/releases/
EXPOSE 80
CMD ["nginx", "-g", "daemon off;"]
