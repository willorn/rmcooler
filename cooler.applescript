-- 红魔散热器 8 Pro · 双击控制面板
-- 编译方式见 README：osacompile -o 红魔散热器.app 这个文件

-- 运行时解析 ~/bin/rmcooler（安装脚本创建的软链），开源后任何人可用
on getBinPath()
	set homePath to POSIX path of (path to home folder)
	return homePath & "bin/rmcooler"
end getBinPath

property binPath : ""

on run
	set options to {"开启（6 档）", "关闭", "1 档（最静）", "3 档", "5 档", "8 档（最猛）", "破坏神 开", "破坏神 关", "智能温控 开", "查看状态"}
	set picked to choose from list options with title "红魔散热器 8 Pro" with prompt "要做什么？" default items {"开启（6 档）"} OK button name "执行" cancel button name "取消"
	if picked is false then return
	set choice to item 1 of picked

	if choice is "开启（6 档）" then
		set shellCmd to "start 6"
	else if choice is "关闭" then
		set shellCmd to "stop"
	else if choice is "1 档（最静）" then
		set shellCmd to "level 1"
	else if choice is "3 档" then
		set shellCmd to "level 3"
	else if choice is "5 档" then
		set shellCmd to "level 5"
	else if choice is "8 档（最猛）" then
		set shellCmd to "level 8"
	else if choice is "破坏神 开" then
		set shellCmd to "boost on"
	else if choice is "破坏神 关" then
		set shellCmd to "boost off"
	else if choice is "智能温控 开" then
		set shellCmd to "auto on"
	else
		set shellCmd to "status"
	end if

	try
		set output to do shell script quoted form of (my getBinPath()) & " " & shellCmd & " 2>&1"
		if output is "" then set output to "完成。"
		display dialog output buttons {"好"} default button 1 with title "红魔散热器" with icon note giving up after 6
	on error errMsg number errNum
		display dialog errMsg buttons {"好"} default button 1 with title "没成功" with icon caution
	end try
end run

on open theFiles
	run
end open
