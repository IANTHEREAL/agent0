FROM nginx:alpine
COPY nginx.conf /etc/nginx/conf.d/default.conf
COPY index.html /usr/share/nginx/html/index.html
COPY install.sh /usr/share/nginx/html/install.sh
COPY releases/ /usr/share/nginx/html/releases/
EXPOSE 80
CMD ["nginx", "-g", "daemon off;"]
