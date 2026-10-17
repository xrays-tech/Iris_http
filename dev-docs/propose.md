# 开发目的
使用Rust语言 Pingora库来实现一个http 和 https 的代理程序

# 工作方式
程序启动的时候在配置目录加载站点配置，站点配置有两种模式
一种是基本转发站点
配置包括 域名，是否开启https，后端IP:port列表（可以含权重），负载均衡模式
当域名精确匹配后，将全部请求代理请求到后端列表里按负载均衡模式取到的值

第二种是OpenStack全自动模式
配置包括 基域名,是否开启https,openstack API地址，认证信息
假设基域名是 rent.do.top 那么当访问的域名是 instance_name-port.rent.do.top 访问的话，先解析出 instance_name 和 port，然后根据 instance_name去查找全局查找实例的floating ip 和 fixed ip。然后将所有请求反向代理到 查出来的ip的port。另外这个查询的结果要缓存一下，缓存个1天吧。查询的过程可以参考 
https://github.com/xrays-tech/Iris_ssh
这个项目。
每个全自动站点的配置，可以支持设置一个防火墙设置目录，目录下有多个INSTANCE.role 的规则配置文件，
比如 INSTANCE.role 里配置了开放本地 80, 8080 端口，支持配置白名单IP和黑名单IP
当你用 INS001-80.rent.do.top 访问的时候（不论是http还是https）就能反向代理http请求到INS001 主机的floating-ip的80端口


代理程序要支持reload命令，可以动态重新加载配置

