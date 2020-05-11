Dim strFilename  
Dim objFSO  
Dim args : Set args = WScript.Arguments

Set objFSO = CreateObject("scripting.filesystemobject") 
strFilename = "AFMER\AFMER-20" & args(0) &".xlsx" 
If objFSO.fileexists(strFilename) Then  
  Call Writefile(strFilename)  
Else  
  wscript.echo "no such file!"  
End If  
Set objFSO = Nothing  

Sub Writefile(ByVal strFilename)  
Dim objExcel  
Dim objWB  
Dim objws  


Set objExcel = CreateObject("Excel.Application")  
Set objWB = objExcel.Workbooks.Open(strFilename)  

For Each objws In objWB.Sheets  
  objws.Copy  
  objExcel.ActiveWorkbook.SaveAs "AFMER\csv\AFMER-20" & args(0) &"-"& objws.Name & ".csv", 6  
  objExcel.ActiveWorkbook.Close False  
Next 

objWB.Close False  
objExcel.Quit  
Set objExcel = Nothing  
End Sub  
