Option Explicit

Const xlXMLSpreadsheet = 46
Const xlCSV = 6

Dim xl, wb, ws

Dim args : Set args = WScript.Arguments

If args.Count <> 1 Then
  WScript.Echo "Syntax: cscript " & WScript.ScriptName & " filename"
  WScript.Quit(1)
End If

Set xl = CreateObject("Excel.Application")
Set wb = xl.Workbooks.Open(args(0))

xl.DisplayAlerts = False
For Each ws In wb.Worksheets
 ws.activate
 wb.SaveAs CreateObject("Scripting.FileSystemObject").GetBaseName(args(0)) _
   & "_" & Replace(ws.Name, " ", "_") & ".csv", xlCSV
Next
xl.DisplayAlerts = True

wb.Close False
xl.Quit
WScript.Quit